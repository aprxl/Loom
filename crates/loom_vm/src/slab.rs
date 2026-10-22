//! `Pool[T]` (design/v6-spools.md): a spool of fixed slots, each one value.
//!
//! The header is five words: the first free slot (a list through the freed
//! slots, 0 at the end), where never-used slots start, where they end, the
//! block of all the slots, and the slot size. A slot is 16-aligned and at
//! least 16 bytes, so a freed slot holds the link.

#![allow(unsafe_code)]

use crate::Trap;
use crate::heap::Heap;

const HEADER: u64 = 40;

const FREE: usize = 0;
const NEXT: usize = 1;
const END: usize = 2;
const BLOCK: usize = 3;
const SLOT: usize = 4;

fn words(header: u64) -> *mut u64 {
    header as *mut u64
}

fn slot_size(size: u64) -> u64 {
    size.max(1).div_ceil(16).max(1) * 16
}

/// `pool.new(slot size, slots) -> pool`.
pub(crate) fn new(heap: &mut Heap, size: u64, slots: u64) -> Result<u64, Trap> {
    let slot = slot_size(size);
    let bytes = slot.checked_mul(slots).ok_or(Trap::OutOfMemory)?;
    let header = heap.alloc(HEADER, 16)?;
    let block = match heap.alloc(bytes, 16) {
        Ok(b) => b,
        Err(e) => {
            heap.free(header, HEADER, 16)?;
            return Err(e);
        }
    };
    // SAFETY: `header` is a new object of `HEADER` bytes.
    unsafe {
        let w = words(header);
        w.add(FREE).write(0);
        w.add(NEXT).write(block);
        w.add(END).write(block + bytes);
        w.add(BLOCK).write(block);
        w.add(SLOT).write(slot);
    }
    Ok(header)
}

/// `pool.alloc(pool) -> slot`: a free slot, or an unravel when none is.
pub(crate) fn alloc(header: u64) -> Result<u64, Trap> {
    // SAFETY: `header` is a pool `new` made and `free_all` hasn't freed.
    unsafe {
        let w = words(header);
        let head = w.add(FREE).read();
        if head != 0 {
            w.add(FREE).write((head as *const u64).read());
            return Ok(head);
        }
        let (next, end, slot) = (w.add(NEXT).read(), w.add(END).read(), w.add(SLOT).read());
        if end - next < slot {
            return Err(Trap::Unravel("the pool is full".into()));
        }
        w.add(NEXT).write(next + slot);
        Ok(next)
    }
}

/// `pool.free(pool, slot)`: the slot is free to give out again.
pub(crate) fn free(header: u64, slot: u64) {
    // SAFETY: as for [`alloc`]; `slot` is one this pool gave out, which the
    // program is finished with (its value dropped).
    unsafe {
        let w = words(header);
        (slot as *mut u64).write(w.add(FREE).read());
        w.add(FREE).write(slot);
    }
}

/// `pool.reset(pool)`: every slot is free again.
pub(crate) fn reset(header: u64) {
    // SAFETY: as for [`alloc`].
    unsafe {
        let w = words(header);
        w.add(FREE).write(0);
        w.add(NEXT).write(w.add(BLOCK).read());
    }
}

/// `pool.drop(pool)`: the pool and its slots.
pub(crate) fn drop_pool(heap: &mut Heap, header: u64) -> Result<(), Trap> {
    // SAFETY: as for [`alloc`].
    let (block, bytes) = unsafe {
        let w = words(header);
        (w.add(BLOCK).read(), w.add(END).read() - w.add(BLOCK).read())
    };
    heap.free(block, bytes, 16)?;
    heap.free(header, HEADER, 16)
}
