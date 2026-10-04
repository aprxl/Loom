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
//! In the checked build every live object is recorded, so a load or store
//! outside one, a free of something that isn't live, and a leak at the end
//! of a run are caught (§5.6).

#![allow(unsafe_code)]

use std::alloc::Layout;
use std::collections::BTreeMap;

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

#[derive(Debug, Default)]
pub(crate) struct Heap {
    /// Bytes allocated and not yet freed.
    live: u64,
    /// With `checked`: every live object, by address, with its size.
    objects: BTreeMap<u64, u64>,
    /// Each size class's free objects.
    free: Vec<Vec<u64>>,
    /// Where the page being carved from is free, to its end.
    next: u64,
    end: u64,
    /// Every page, to give back when the heap goes.
    pages: Vec<u64>,
}

impl Drop for Heap {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(PAGE as usize, SMALL_ALIGN as usize).expect("a valid layout");
        for &p in &self.pages {
            // SAFETY: each page was allocated with this layout, and nothing
            // of the program is left to use it.
            unsafe { std::alloc::dealloc(p as *mut u8, layout) };
        }
    }
}

impl Heap {
    fn layout(size: u64, align: u64) -> Option<Layout> {
        Layout::from_size_align(usize::try_from(size).ok()?, usize::try_from(align.max(1)).ok()?).ok()
    }

    fn is_small(size: u64, align: u64) -> bool {
        size <= SMALL && align <= SMALL_ALIGN
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
        if size > LIMIT - self.live {
            return Err(Trap::OutOfMemory);
        }
        let ptr = if Self::is_small(size, align) { self.alloc_small(size)? } else { Self::alloc_big(size, align)? };
        self.live += size;
        if cfg!(feature = "checked") {
            self.objects.insert(ptr, size);
        }
        Ok(ptr)
    }

    fn alloc_small(&mut self, size: u64) -> Result<u64, Trap> {
        let (class, bytes) = class_of(size);
        if self.free.len() < CLASSES {
            self.free.resize_with(CLASSES, Vec::new);
        }
        if let Some(p) = self.free[class].pop() {
            return Ok(p);
        }
        if self.end - self.next < bytes {
            let layout = Layout::from_size_align(PAGE as usize, SMALL_ALIGN as usize).expect("a valid layout");
            // SAFETY: the layout's size isn't zero.
            let page = unsafe { std::alloc::alloc(layout) } as u64;
            if page == 0 {
                return Err(Trap::OutOfMemory);
            }
            self.pages.push(page);
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
        if cfg!(feature = "checked") && self.objects.remove(&ptr) != Some(size) {
            return Err(Trap::Crash(format!("freeing {size} bytes at {ptr:#x}, which isn't an object of that size")));
        }
        if Self::is_small(size, align) {
            let (class, _) = class_of(size);
            self.free[class].push(ptr);
        } else {
            let layout = Self::layout(size, align).ok_or_else(|| Trap::Crash("freeing with a bad layout".into()))?;
            // SAFETY: `ptr` was allocated with this layout (the compiler
            // frees what it allocated, with the same size and alignment; the
            // checked build verifies it above) and is freed once.
            unsafe { std::alloc::dealloc(ptr as *mut u8, layout) };
        }
        self.live -= size;
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
        if new > old && new - old > LIMIT - self.live {
            return Err(Trap::OutOfMemory);
        }
        if cfg!(feature = "checked") && self.objects.get(&ptr) != Some(&old) {
            return Err(Trap::Crash(format!("growing {old} bytes at {ptr:#x}, which isn't an object of that size")));
        }
        // Still the same class: it fits where it is.
        if Self::is_small(old, align) && Self::is_small(new, align) && class_of(old).1 == class_of(new).1 {
            if new > old && new - old > LIMIT - self.live {
                return Err(Trap::OutOfMemory);
            }
            self.live = self.live - old + new;
            if cfg!(feature = "checked") {
                self.objects.insert(ptr, new);
            }
            return Ok(ptr);
        }
        if !Self::is_small(old, align) && !Self::is_small(new, align) {
            let layout = Self::layout(old, align).ok_or(Trap::OutOfMemory)?;
            let size = usize::try_from(new).map_err(|_| Trap::OutOfMemory)?;
            if Self::layout(new, align).is_none() {
                return Err(Trap::OutOfMemory);
            }
            // SAFETY: `ptr` is a live object allocated with `layout`, and the
            // new size is a valid size for its alignment (checked above).
            let moved = unsafe { std::alloc::realloc(ptr as *mut u8, layout, size) } as u64;
            if moved == 0 {
                return Err(Trap::OutOfMemory);
            }
            self.live = self.live - old + new;
            if cfg!(feature = "checked") {
                self.objects.remove(&ptr);
                self.objects.insert(moved, new);
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

    /// With `checked`: whether `len` bytes at `addr` are inside one live
    /// object.
    pub fn holds(&self, addr: u64, len: u64) -> bool {
        match self.objects.range(..=addr).next_back() {
            Some((&start, &size)) => addr.checked_add(len).is_some_and(|end| end <= start + size),
            None => false,
        }
    }

    /// With `checked`: how many objects are live, and their bytes.
    pub fn leaked(&self) -> (usize, u64) {
        (self.objects.len(), self.objects.values().sum())
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
        assert_eq!(heap.live, 0);
    }

    #[test]
    fn realloc_past_the_limit_is_out_of_memory() {
        let mut heap = Heap::default();
        let p = heap.alloc(64, 8).expect("allocates");
        assert_eq!(heap.realloc(p, 64, LIMIT + 1, 8), Err(Trap::OutOfMemory));
        // The same class: a small growth that would pass the limit.
        heap.live = LIMIT;
        assert_eq!(heap.realloc(p, 33, 48, 8), Err(Trap::OutOfMemory));
        heap.live = 64;
        heap.free(p, 64, 8).expect("frees");
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
