//! The heap: where list, string and map buffers, boxes and streams live
//! (design/vm.md §5.5).
//!
//! Every object is freed by its owner, with the size and alignment it was
//! allocated with (a list knows its capacity and its element type), so
//! objects carry no header. Small objects (up to 32 KiB, aligned to at most
//! 16) come in size classes: 16 bytes apart up to 256, then four classes
//! per doubling. Each class has a free list, and new objects are carved out
//! of 64 KiB pages; the class an object is in follows from its size, which
//! its owner passes back. Bigger ones go to Rust's global allocator. Past
//! [`LIMIT`] bytes live at once, allocating unravels with `out of memory`,
//! as on Lua.
//!
//! A [`Heap`] is one worker's cache: its free lists and its page, and a
//! share of the bytes the limit allows, taken from the program's total in
//! chunks, so allocating and freeing need no lock. The heaps of a program's
//! workers share a [`Shared`]: the total, the pages, and the lists a heap
//! gave up when it had freed much more than it allocated (a buffer made on
//! one worker and freed on another), which a heap with an empty list takes
//! before it carves.
//!
//! In the checked build every live object is recorded, so a load or store
//! outside one, a free of something that isn't live, and a leak at the end
//! of a run are caught (§5.6).

#![allow(unsafe_code)]

use std::alloc::Layout;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::Trap;

/// The most bytes a program may have allocated at once.
pub(crate) const LIMIT: u64 = 8 << 30;

/// The biggest object a size class holds.
const SMALL: u64 = 32 * 1024;
/// The alignment of every object in a size class.
const SMALL_ALIGN: u64 = 16;
/// The size of the pages small objects are carved from.
const PAGE: u64 = 64 * 1024;
/// How many size classes there are (see [`class_of`]).
const CLASSES: usize = 44;
/// How many bytes of the limit a heap takes at a time, and keeps when it has
/// freed more than twice as many.
const CHUNK: u64 = 16 * 1024;

/// The size class of `size` bytes (`0 < size <= SMALL`): its number, and
/// the size of its objects.
fn class_of(size: u64) -> (usize, u64) {
    if size <= 256 {
        let i = size.div_ceil(16);
        return (i as usize - 1, i * 16);
    }
    // Between 2^p and 2^(p+1), four classes, 2^(p-2) apart.
    let p = 63 - u64::from((size - 1).leading_zeros());
    let step = 1u64 << (p - 2);
    let k = (size - (1 << p)).div_ceil(step);
    (16 + (p as usize - 8) * 4 + (k as usize - 1), (1 << p) + k * step)
}

fn page_layout() -> Layout {
    Layout::from_size_align(PAGE as usize, SMALL_ALIGN as usize).expect("a valid layout")
}

/// What a program's heaps share.
#[derive(Debug)]
pub(crate) struct Shared {
    /// Bytes in live objects, plus what the heaps hold in reserve.
    live: AtomicU64,
    /// The most `live` may be: [`LIMIT`], or what compile-time code is
    /// given.
    limit: AtomicU64,
    /// With `checked`: every live object, by address, with its size.
    objects: Mutex<BTreeMap<u64, u64>>,
    /// Every page, to give back when the program's memory goes.
    pages: Mutex<Vec<u64>>,
    /// Free lists heaps gave up, by class (each is a list through the
    /// objects, as in [`Heap::free`]).
    spilled: Mutex<[Vec<u64>; CLASSES]>,
    /// The classes `spilled` has a list of.
    pending: AtomicU64,
}

impl Default for Shared {
    fn default() -> Shared {
        Shared {
            live: AtomicU64::new(0),
            limit: AtomicU64::new(LIMIT),
            objects: Mutex::default(),
            pages: Mutex::default(),
            spilled: Mutex::new(std::array::from_fn(|_| Vec::new())),
            pending: AtomicU64::new(0),
        }
    }
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for Shared {
    fn drop(&mut self) {
        let layout = page_layout();
        for &p in locked(&self.pages).iter() {
            // SAFETY: each page was allocated with this layout, and no heap
            // is left to use it.
            unsafe { std::alloc::dealloc(p as *mut u8, layout) };
        }
    }
}

#[derive(Debug)]
pub(crate) struct Heap {
    shared: Arc<Shared>,
    /// Bytes of the limit this heap holds that no object of its has: an
    /// allocation spends them, a free adds to them.
    credit: u64,
    /// Each size class's free objects, as a list through the objects
    /// themselves (the first word of a free one is the next's address, 0
    /// at the end): every object is at least 16 bytes.
    free: [u64; CLASSES],
    /// Where the page being carved from is free, to its end.
    next: u64,
    end: u64,
}

impl Default for Heap {
    fn default() -> Heap {
        Heap::sharing(&Arc::new(Shared::default()))
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        self.shared.live.fetch_sub(self.credit, Relaxed);
        self.spill();
    }
}

impl Heap {
    /// Writes a `String` of `len` bytes at `dst`, which `fill` writes in
    /// place: no buffer of its own to copy from.
    pub(crate) fn new_string_with(&mut self, len: usize, dst: u64, fill: impl FnOnce(&mut [u8])) -> Result<(), Trap> {
        let ptr = self.alloc(len as u64, 1)?;
        // SAFETY: `ptr` is a new object of `len` bytes (or, for none, never
        // read), which nothing else refers to; `dst` is where the compiler
        // wants the string: three words of memory it owns.
        unsafe {
            if len > 0 {
                fill(std::slice::from_raw_parts_mut(ptr as *mut u8, len));
            }
            let d = dst as *mut u64;
            d.write_unaligned(ptr);
            d.add(1).write_unaligned(len as u64);
            d.add(2).write_unaligned(len as u64);
        }
        Ok(())
    }

    /// Writes a `String` holding `text` at `dst`.
    pub(crate) fn new_string(&mut self, text: &[u8], dst: u64) -> Result<(), Trap> {
        let len = text.len() as u64;
        let ptr = self.alloc(len, 1)?;
        // SAFETY: `ptr` is a new object of `len` bytes (or, for none, never
        // read), and `dst` is where the compiler wants the string: three
        // words of memory it owns.
        unsafe {
            if len > 0 {
                std::ptr::copy_nonoverlapping(text.as_ptr(), ptr as *mut u8, text.len());
            }
            let d = dst as *mut u64;
            d.write_unaligned(ptr);
            d.add(1).write_unaligned(len);
            d.add(2).write_unaligned(len);
        }
        Ok(())
    }

    /// A heap of its own with the program's total, pages and spilled lists.
    pub fn sharing(shared: &Arc<Shared>) -> Heap {
        Heap { shared: Arc::clone(shared), credit: 0, free: [0; CLASSES], next: 0, end: 0 }
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Allows `bytes` to be live at once (at most [`LIMIT`]): for
    /// compile-time code, before it allocates anything.
    pub fn set_limit(&mut self, bytes: u64) {
        self.shared.limit.store(bytes.min(LIMIT), Relaxed);
    }

    fn layout(size: u64, align: u64) -> Option<Layout> {
        Layout::from_size_align(usize::try_from(size).ok()?, usize::try_from(align.max(1)).ok()?).ok()
    }

    fn is_small(size: u64, align: u64) -> bool {
        size <= SMALL && align <= SMALL_ALIGN
    }

    /// Spends `n` bytes of the limit, taking more from the program's total
    /// when this heap's share is short: a chunk, or what is left if that is
    /// less. The first heap past the limit gets `out of memory`.
    #[inline(always)]
    fn take(&mut self, n: u64) -> Result<(), Trap> {
        if n > self.credit {
            self.reserve(n)?;
        }
        self.credit -= n;
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn reserve(&mut self, n: u64) -> Result<(), Trap> {
        let need = n - self.credit;
        let limit = self.shared.limit.load(Relaxed);
        let mut live = self.shared.live.load(Relaxed);
        loop {
            let room = limit.saturating_sub(live);
            if need > room {
                return Err(Trap::OutOfMemory);
            }
            let grab = need.max(CHUNK).min(room);
            match self.shared.live.compare_exchange_weak(live, live + grab, Relaxed, Relaxed) {
                Ok(_) => {
                    self.credit += grab;
                    return Ok(());
                }
                Err(now) => live = now,
            }
        }
    }

    /// Gives back `n` bytes of the limit; a heap that ends up with more
    /// than two chunks gives all but one back to the total, and its lists to
    /// the others.
    #[inline(always)]
    fn give(&mut self, n: u64) {
        self.credit += n;
        if self.credit > 2 * CHUNK {
            self.release();
        }
    }

    #[cold]
    #[inline(never)]
    fn release(&mut self) {
        self.shared.live.fetch_sub(self.credit - CHUNK, Relaxed);
        self.credit = CHUNK;
        self.spill();
    }

    /// Hands the free lists to the other heaps, if there are any.
    fn spill(&mut self) {
        if Arc::strong_count(&self.shared) == 1 {
            return;
        }
        let mut spilled = None;
        for (class, head) in self.free.iter_mut().enumerate() {
            if *head != 0 {
                let lists = spilled.get_or_insert_with(|| locked(&self.shared.spilled));
                lists[class].push(*head);
                self.shared.pending.fetch_or(1 << class, Relaxed);
                *head = 0;
            }
        }
    }

    /// A list another heap gave up, for `class`.
    fn take_spilled(&self, class: usize) -> Option<u64> {
        let mut lists = locked(&self.shared.spilled);
        let head = lists[class].pop();
        if lists[class].is_empty() {
            self.shared.pending.fetch_and(!(1 << class), Relaxed);
        }
        head
    }

    /// `size` bytes aligned to `align`. Nothing is allocated for zero
    /// bytes: the address is `align`, never null and never dereferenced.
    // SAFETY: no other symbol in the program has this name, and nothing calls
    // it through the name.
    #[allow(unsafe_code)]
    #[unsafe(export_name = "loom_treadle_heap_alloc")]
    pub fn alloc(&mut self, size: u64, align: u64) -> Result<u64, Trap> {
        if size == 0 {
            return Ok(align.max(1));
        }
        self.take(size)?;
        let ptr = if Self::is_small(size, align) { self.alloc_small(size) } else { Self::alloc_big(size, align) };
        let ptr = match ptr {
            Ok(ptr) => ptr,
            Err(trap) => {
                self.give(size);
                return Err(trap);
            }
        };
        if cfg!(feature = "checked") {
            locked(&self.shared.objects).insert(ptr, size);
        }
        Ok(ptr)
    }

    fn alloc_small(&mut self, size: u64) -> Result<u64, Trap> {
        let (class, bytes) = class_of(size);
        let head = self.free[class];
        if head != 0 {
            // SAFETY: a free object of this class: at least 16 bytes the
            // heap owns, whose first word is the next one (see `free`).
            self.free[class] = unsafe { (head as *const u64).read() };
            return Ok(head);
        }
        self.carve(class, bytes)
    }

    /// An object of `class` when its free list is empty: from a list
    /// another heap gave up, or new, from the page.
    fn carve(&mut self, class: usize, bytes: u64) -> Result<u64, Trap> {
        if self.shared.pending.load(Relaxed) & (1 << class) != 0
            && let Some(head) = self.take_spilled(class)
        {
            // SAFETY: the head of a list of free objects of this class.
            self.free[class] = unsafe { (head as *const u64).read() };
            return Ok(head);
        }
        if self.end - self.next < bytes {
            // SAFETY: the layout's size isn't zero.
            let page = unsafe { std::alloc::alloc(page_layout()) } as u64;
            if page == 0 {
                return Err(Trap::OutOfMemory);
            }
            locked(&self.shared.pages).push(page);
            self.next = page;
            self.end = page + PAGE;
        }
        let p = self.next;
        self.next += bytes;
        Ok(p)
    }

    fn alloc_big(size: u64, align: u64) -> Result<u64, Trap> {
        let layout = Self::layout(size, align).ok_or(Trap::OutOfMemory)?;
        // SAFETY: the layout's size isn't zero.
        let ptr = unsafe { std::alloc::alloc(layout) } as u64;
        if ptr == 0 {
            return Err(Trap::OutOfMemory);
        }
        Ok(ptr)
    }

    /// Frees what [`alloc`](Self::alloc) gave for `size` and `align`.
    // SAFETY: no other symbol in the program has this name, and nothing calls
    // it through the name.
    #[allow(unsafe_code)]
    #[unsafe(export_name = "loom_treadle_heap_free")]
    pub fn free(&mut self, ptr: u64, size: u64, align: u64) -> Result<(), Trap> {
        if size == 0 {
            return Ok(());
        }
        if cfg!(feature = "checked") && locked(&self.shared.objects).remove(&ptr) != Some(size) {
            return Err(Trap::Crash(format!("freeing {size} bytes at {ptr:#x}, which isn't an object of that size")));
        }
        if Self::is_small(size, align) {
            let (class, _) = class_of(size);
            // SAFETY: `ptr` is a live object of this class (at least 16
            // bytes, 16-aligned) being given back: nothing reads it now.
            unsafe { (ptr as *mut u64).write(self.free[class]) };
            self.free[class] = ptr;
        } else {
            let layout = Self::layout(size, align).ok_or_else(|| Trap::Crash("freeing with a bad layout".into()))?;
            // SAFETY: `ptr` was allocated with this layout (the compiler
            // frees what it allocated, with the same size and alignment; the
            // checked build verifies it above) and is freed once.
            unsafe { std::alloc::dealloc(ptr as *mut u8, layout) };
        }
        self.give(size);
        Ok(())
    }

    /// Moves an object of `old` bytes to one of `new` bytes, keeping what
    /// fits in both.
    // SAFETY: no other symbol in the program has this name, and nothing calls
    // it through the name.
    #[allow(unsafe_code)]
    #[unsafe(export_name = "loom_treadle_heap_realloc")]
    pub fn realloc(&mut self, ptr: u64, old: u64, new: u64, align: u64) -> Result<u64, Trap> {
        if old == 0 {
            return self.alloc(new, align);
        }
        if new == 0 {
            self.free(ptr, old, align)?;
            return Ok(align.max(1));
        }
        if cfg!(feature = "checked") && locked(&self.shared.objects).get(&ptr) != Some(&old) {
            return Err(Trap::Crash(format!("growing {old} bytes at {ptr:#x}, which isn't an object of that size")));
        }
        // Still the same class: it fits where it is.
        if Self::is_small(old, align) && Self::is_small(new, align) && class_of(old).1 == class_of(new).1 {
            self.resize_account(old, new)?;
            if cfg!(feature = "checked") {
                locked(&self.shared.objects).insert(ptr, new);
            }
            return Ok(ptr);
        }
        if !Self::is_small(old, align) && !Self::is_small(new, align) {
            let layout = Self::layout(old, align).ok_or(Trap::OutOfMemory)?;
            let size = usize::try_from(new).map_err(|_| Trap::OutOfMemory)?;
            if Self::layout(new, align).is_none() {
                return Err(Trap::OutOfMemory);
            }
            self.resize_account(old, new)?;
            // SAFETY: `ptr` is a live object allocated with `layout`, and the
            // new size is a valid size for its alignment (checked above).
            let moved = unsafe { std::alloc::realloc(ptr as *mut u8, layout, size) } as u64;
            if moved == 0 {
                self.resize_account(new, old).expect("giving bytes back");
                return Err(Trap::OutOfMemory);
            }
            if cfg!(feature = "checked") {
                let mut objects = locked(&self.shared.objects);
                objects.remove(&ptr);
                objects.insert(moved, new);
            }
            return Ok(moved);
        }
        // From one kind to the other: a new object, and a copy.
        let moved = self.alloc(new, align)?;
        // SAFETY: both are live objects, of at least the bytes copied.
        unsafe { std::ptr::copy_nonoverlapping(ptr as *const u8, moved as *mut u8, old.min(new) as usize) };
        self.free(ptr, old, align)?;
        Ok(moved)
    }

    /// An object of `old` bytes is now one of `new`: spends or gives back
    /// the difference.
    fn resize_account(&mut self, old: u64, new: u64) -> Result<(), Trap> {
        if new > old {
            self.take(new - old)
        } else {
            self.give(old - new);
            Ok(())
        }
    }

    /// With `checked`: whether `len` bytes at `addr` are inside one live
    /// object.
    pub fn holds(&self, addr: u64, len: u64) -> bool {
        match locked(&self.shared.objects).range(..=addr).next_back() {
            Some((&start, &size)) => addr.checked_add(len).is_some_and(|end| end <= start + size),
            None => false,
        }
    }

    /// With `checked`: the sizes of the live objects, smallest first.
    pub fn leaked_sizes(&self) -> Vec<u64> {
        let mut sizes: Vec<u64> = locked(&self.shared.objects).values().copied().collect();
        sizes.sort_unstable();
        sizes
    }

    /// With `checked`: how many objects are live, and their bytes.
    pub fn leaked(&self) -> (usize, u64) {
        let objects = locked(&self.shared.objects);
        (objects.len(), objects.values().sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realloc_keeps_the_bytes_across_the_small_and_big_boundary() {
        let mut heap = Heap::default();
        let small = 100;
        let big = SMALL + 1000;
        let p = heap.alloc(small, 8).expect("allocates");
        // SAFETY: `p` is a live object of `small` bytes.
        unsafe { (0..small).for_each(|i| (p as *mut u8).add(i as usize).write(i as u8)) };
        let q = heap.realloc(p, small, big, 8).expect("grows to a big object");
        // SAFETY: `q` holds at least `small` bytes.
        assert!((0..small).all(|i| unsafe { (q as *const u8).add(i as usize).read() } == i as u8));
        let r = heap.realloc(q, big, small, 8).expect("shrinks to a small object");
        // SAFETY: `r` holds `small` bytes.
        assert!((0..small).all(|i| unsafe { (r as *const u8).add(i as usize).read() } == i as u8));
        heap.free(r, small, 8).expect("frees");
        assert_eq!(heap.shared.live.load(Relaxed), heap.credit);
    }

    #[test]
    fn realloc_past_the_limit_is_out_of_memory() {
        let mut heap = Heap::default();
        let p = heap.alloc(64, 8).expect("allocates");
        assert_eq!(heap.realloc(p, 64, LIMIT + 1, 8), Err(Trap::OutOfMemory));
        // The same class: a small growth that would pass the limit.
        heap.free(p, 64, 8).expect("frees");
        // The same class: a small growth that would pass the limit.
        let mut heap = Heap::default();
        heap.set_limit(40);
        let q = heap.alloc(33, 8).expect("allocates");
        assert_eq!(heap.realloc(q, 33, 48, 8), Err(Trap::OutOfMemory));
    }

    #[test]
    fn the_limit_is_exact_for_one_heap() {
        let mut heap = Heap::default();
        heap.set_limit(1000);
        let p = heap.alloc(600, 8).expect("fits");
        assert_eq!(heap.alloc(401, 8), Err(Trap::OutOfMemory));
        let q = heap.alloc(400, 8).expect("fits exactly");
        heap.free(p, 600, 8).expect("frees");
        heap.free(q, 400, 8).expect("frees");
        assert!(heap.alloc(1000, 8).is_ok());
    }

    #[test]
    fn a_free_on_another_heap_is_reused() {
        let mut a = Heap::default();
        let mut b = Heap::sharing(a.shared());
        let ptrs: Vec<u64> = (0..CHUNK / 64 * 3).map(|_| a.alloc(48, 8).expect("allocates")).collect();
        for &p in &ptrs {
            b.free(p, 48, 8).expect("frees");
        }
        let again = a.alloc(48, 8).expect("allocates");
        assert!(ptrs.contains(&again), "a heap that has freed far more than it allocated gives its lists away");
    }

    #[test]
    fn classes_hold_their_sizes_and_are_numbered_in_order() {
        let mut last = (0, 0);
        for size in 1..=SMALL {
            let (class, bytes) = class_of(size);
            assert!(bytes >= size && bytes % 16 == 0, "{size}: {bytes}");
            assert!(class < CLASSES, "{size}: class {class}");
            assert!(class == last.0 || class == last.0 + 1 || size == 1, "{size}: {class} after {}", last.0);
            assert!(class > last.0 || bytes == last.1 || size == 1);
            last = (class, bytes);
        }
        assert_eq!(class_of(SMALL), (CLASSES - 1, SMALL));
    }
}
