//! Atomic leaves use only the caller's argument window and memory owned by
//! the Loom program. They never borrow the shared native context.

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering::SeqCst};

use crate::{Trap, heap::Heap, ints, natives::LocalFn};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Op {
    Load,
    Store,
    Swap,
    Cas,
    Add,
    Sub,
}

pub(crate) fn resolve(name: &str) -> Option<LocalFn> {
    Some(match name {
        "atomic.load" => |h, a, _| run(h, a, Op::Load),
        "atomic.store" => |h, a, _| run(h, a, Op::Store),
        "atomic.swap" => |h, a, _| run(h, a, Op::Swap),
        "atomic.cas" => |h, a, _| run(h, a, Op::Cas),
        "atomic.add" => |h, a, _| run(h, a, Op::Add),
        "atomic.sub" => |h, a, _| run(h, a, Op::Sub),
        _ => return None,
    })
}

pub(crate) fn run(heap: &Heap, args: &mut [u64], op: Op) -> Result<(), Trap> {
    let spec_at = match op {
        Op::Load => 1,
        Op::Cas => 3,
        _ => 2,
    };
    let spec = loom_isa::IntSpec::unpack(args[spec_at])
        .ok_or_else(|| Trap::Crash("loom internal: a bad atomic type".into()))?;
    let addr = args[0];
    macro_rules! access {
        ($atomic:ty, $word:ty) => {{
            let size = std::mem::size_of::<$atomic>() as u64;
            if !addr.is_multiple_of(std::mem::align_of::<$atomic>() as u64) {
                return Err(Trap::Crash("loom internal: a misaligned atomic".into()));
            }
            if cfg!(feature = "checked") && !(heap.holds(addr, size) || crate::stack::in_some_stack(addr, size)) {
                return Err(Trap::Crash(format!("loom internal: an atomic at {addr:#x} that isn't live")));
            }
            // SAFETY: the compiler supplies an Atomic of this exact width.
            // Alignment is checked above, and the checked build validates
            // liveness. Its owner outlives all task captures; concurrent
            // accesses use this same atomic width, never ordinary loads.
            let cell = unsafe { <$atomic>::from_ptr(addr as *mut $word) };
            let normalize = |v: $word| ints::wrap(u64::from(v), spec);
            match op {
                Op::Load => args[0] = normalize(cell.load(SeqCst)),
                Op::Store => cell.store(args[1] as $word, SeqCst),
                Op::Swap => args[0] = normalize(cell.swap(args[1] as $word, SeqCst)),
                Op::Cas => {
                    let result = cell.compare_exchange(args[1] as $word, args[2] as $word, SeqCst, SeqCst);
                    args[0] = u64::from(result.is_ok());
                    args[1] = normalize(result.unwrap_or_else(|found| found));
                }
                Op::Add | Op::Sub => {
                    let mut before = cell.load(SeqCst);
                    loop {
                        let old = normalize(before);
                        let after = match op {
                            Op::Add => ints::add_t(old, args[1], spec),
                            _ => ints::sub_t(old, args[1], spec),
                        }
                        .map_err(|_| Trap::Overflow)?;
                        // Overflow returns before any write. A failed CAS
                        // retries against the newly observed value.
                        match cell.compare_exchange_weak(before, after as $word, SeqCst, SeqCst) {
                            Ok(_) => {
                                args[0] = old;
                                break;
                            }
                            Err(found) => before = found,
                        }
                    }
                }
            }
        }};
    }
    match spec.bits {
        8 => access!(AtomicU8, u8),
        16 => access!(AtomicU16, u16),
        32 => access!(AtomicU32, u32),
        64 => access!(AtomicU64, u64),
        _ => return Err(Trap::Crash("loom internal: an unsupported atomic width".into())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_checked_updates_do_not_lose_increments() {
        let mut heap = Heap::default();
        let addr = heap.alloc(8, 8).expect("allocated");
        let spec = loom_isa::IntSpec::new(64, false).pack();
        run(&heap, &mut [addr, 0, spec], Op::Store).expect("initialized");
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let heap = &heap;
                scope.spawn(move || {
                    for _ in 0..2000 {
                        run(heap, &mut [addr, 1, spec], Op::Add).expect("fits");
                    }
                });
            }
        });
        let mut load = [addr, spec];
        run(&heap, &mut load, Op::Load).expect("loaded");
        assert_eq!(load[0], 16000);
        heap.free(addr, 8, 8).expect("freed");
    }

    #[test]
    fn checked_updates_leave_each_width_unchanged_on_overflow() {
        let mut heap = Heap::default();
        for bits in [8, 16, 32, 64] {
            for signed in [false, true] {
                let spec = loom_isa::IntSpec::new(bits, signed);
                let size = u64::from(bits) / 8;
                let addr = heap.alloc(size, size).expect("allocated");
                let max = if signed { (1u64 << (bits - 1)) - 1 } else { u64::MAX >> (64 - bits) };
                run(&heap, &mut [addr, max, spec.pack()], Op::Store).expect("stored");
                assert_eq!(run(&heap, &mut [addr, 1, spec.pack()], Op::Add), Err(Trap::Overflow));
                let mut load = [addr, spec.pack()];
                run(&heap, &mut load, Op::Load).expect("loaded");
                assert_eq!(load[0], max);
                let min = if signed { (!0u64) << (bits - 1) } else { 0 };
                run(&heap, &mut [addr, min, spec.pack()], Op::Store).expect("stored");
                assert_eq!(run(&heap, &mut [addr, 1, spec.pack()], Op::Sub), Err(Trap::Overflow));
                let mut load = [addr, spec.pack()];
                run(&heap, &mut load, Op::Load).expect("loaded");
                assert_eq!(load[0], min);
                heap.free(addr, size, size).expect("freed");
            }
        }
    }

    #[test]
    fn malformed_width_is_rejected_before_memory_access() {
        let heap = Heap::default();
        for bits in [1, 9, 17, 33, 63] {
            let spec = loom_isa::IntSpec::new(bits, false);
            assert!(matches!(run(&heap, &mut [0, spec.pack()], Op::Load), Err(Trap::Crash(_))));
        }
    }
}
