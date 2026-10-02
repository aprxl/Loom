//! The heap: where list, string and map buffers, boxes and streams live
//! (design/vm.md §5.5).
//!
//! Every object is freed by its owner, with the size and alignment it was
//! allocated with (a list knows its capacity and its element type), so
//! objects carry no header. Allocation goes to Rust's global allocator.
//! Past [`LIMIT`] bytes live at once, allocating unravels with `out of
//! memory`, as on Lua.
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

#[derive(Debug, Default)]
pub(crate) struct Heap {
    /// Bytes allocated and not yet freed.
    live: u64,
    /// With `checked`: every live object, by address, with its size.
    objects: BTreeMap<u64, u64>,
}

impl Heap {
    fn layout(size: u64, align: u64) -> Option<Layout> {
        Layout::from_size_align(usize::try_from(size).ok()?, usize::try_from(align.max(1)).ok()?).ok()
    }

    /// `size` bytes aligned to `align`. Nothing is allocated for zero
    /// bytes: the address is `align`, never null and never dereferenced.
    pub fn alloc(&mut self, size: u64, align: u64) -> Result<u64, Trap> {
        if size == 0 {
            return Ok(align.max(1));
        }
        if size > LIMIT - self.live {
            return Err(Trap::OutOfMemory);
        }
        let layout = Self::layout(size, align).ok_or(Trap::OutOfMemory)?;
        // SAFETY: the layout's size isn't zero.
        let ptr = unsafe { std::alloc::alloc(layout) } as u64;
        if ptr == 0 {
            return Err(Trap::OutOfMemory);
        }
        self.live += size;
        if cfg!(feature = "checked") {
            self.objects.insert(ptr, size);
        }
        Ok(ptr)
    }

    /// Takes over `bytes`'s buffer as an object of its capacity (aligned to
    /// 1): its address and size. With no capacity, nothing is taken.
    pub fn adopt(&mut self, mut bytes: Vec<u8>) -> Result<(u64, u64), Trap> {
        let cap = bytes.capacity() as u64;
        if cap == 0 {
            return Ok((1, 0));
        }
        if cap > LIMIT - self.live {
            return Err(Trap::OutOfMemory);
        }
        let ptr = bytes.as_mut_ptr() as u64;
        // A `Vec<u8>`'s buffer was allocated with the global allocator for
        // `cap` bytes aligned to 1: what `free` gives back.
        std::mem::forget(bytes);
        self.live += cap;
        if cfg!(feature = "checked") {
            self.objects.insert(ptr, cap);
        }
        Ok((ptr, cap))
    }

    /// Frees what [`alloc`](Self::alloc) gave for `size` and `align`.
    pub fn free(&mut self, ptr: u64, size: u64, align: u64) -> Result<(), Trap> {
        if size == 0 {
            return Ok(());
        }
        if cfg!(feature = "checked") && self.objects.remove(&ptr) != Some(size) {
            return Err(Trap::Crash(format!("freeing {size} bytes at {ptr:#x}, which isn't an object of that size")));
        }
        let layout = Self::layout(size, align).ok_or_else(|| Trap::Crash("freeing with a bad layout".into()))?;
        // SAFETY: `ptr` was allocated with this layout (the compiler frees
        // what it allocated, with the same size and alignment; the checked
        // build verifies it above) and is freed once.
        unsafe { std::alloc::dealloc(ptr as *mut u8, layout) };
        self.live -= size;
        Ok(())
    }

    /// Moves an object of `old` bytes to one of `new` bytes, keeping what
    /// fits in both.
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
        let layout = Self::layout(old, align).ok_or(Trap::OutOfMemory)?;
        let size = usize::try_from(new).map_err(|_| Trap::OutOfMemory)?;
        if Self::layout(new, align).is_none() {
            return Err(Trap::OutOfMemory);
        }
        // SAFETY: `ptr` is a live object allocated with `layout`, and the
        // new size is a valid size for its alignment (checked just above).
        let moved = unsafe { std::alloc::realloc(ptr as *mut u8, layout, size) } as u64;
        if moved == 0 {
            return Err(Trap::OutOfMemory);
        }
        self.live = self.live - old + new;
        if cfg!(feature = "checked") {
            self.objects.remove(&ptr);
            self.objects.insert(moved, new);
        }
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
