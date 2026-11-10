//! A table on cache lines of its own.
//!
//! The code, the function and native tables and the text literals are read
//! by every worker's dispatch loop and never written while the program
//! runs. If one of them shares a cache line with something a worker writes
//! (its output line, its heap), each write takes the line from all the
//! other workers, which then read it again from memory: `bench/par_print`
//! ran a third slower on 8 workers when a small program's tables happened
//! to be allocated beside a worker's buffers. So a table starts on a line's
//! boundary and takes whole lines, wherever the allocator puts it.

#![allow(unsafe_code)]

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// The size of a cache line (64 bytes on x86-64 and most others; where a
/// line is bigger, two tables can still meet on one).
const LINE: usize = 64;

/// A slice that shares no cache line with anything else.
pub(crate) struct Lines<T> {
    items: NonNull<T>,
    len: usize,
}

// SAFETY: a `Lines` owns its items, as a `Box<[T]>` does.
unsafe impl<T: Send> Send for Lines<T> {}
// SAFETY: as above.
unsafe impl<T: Sync> Sync for Lines<T> {}

impl<T> Lines<T> {
    /// Whole lines for `len` items (at least one: every allocation has a size).
    fn layout(len: usize) -> Layout {
        let bytes = (len * size_of::<T>()).next_multiple_of(LINE).max(LINE);
        Layout::from_size_align(bytes, LINE.max(align_of::<T>())).expect("a table's size fits")
    }
}

impl<T> From<Vec<T>> for Lines<T> {
    fn from(mut items: Vec<T>) -> Lines<T> {
        let len = items.len();
        let layout = Self::layout(len);
        // SAFETY: the layout has a size.
        let Some(to) = NonNull::new(unsafe { alloc(layout) }.cast::<T>()) else { handle_alloc_error(layout) };
        // SAFETY: `to` has room for `len` items, which move there: the
        // vector is left with none to drop.
        unsafe {
            to.copy_from_nonoverlapping(NonNull::new_unchecked(items.as_mut_ptr()), len);
            items.set_len(0);
        }
        Lines { items: to, len }
    }
}

impl<T> Deref for Lines<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: `len` items, written by `from`.
        unsafe { std::slice::from_raw_parts(self.items.as_ptr(), self.len) }
    }
}

impl<T> DerefMut for Lines<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as for `deref`, and nothing else reaches them.
        unsafe { std::slice::from_raw_parts_mut(self.items.as_ptr(), self.len) }
    }
}

impl<T> Drop for Lines<T> {
    fn drop(&mut self) {
        // SAFETY: the items are dropped once, and the memory was allocated
        // with this layout.
        unsafe {
            std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(self.items.as_ptr(), self.len));
            dealloc(self.items.as_ptr().cast(), Self::layout(self.len));
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Lines<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_takes_whole_lines() {
        for len in [0, 1, 7, 8, 9, 100] {
            let table = Lines::from((0..len as u64).collect::<Vec<_>>());
            assert_eq!(table.as_ptr() as usize % LINE, 0);
            assert_eq!(Lines::<u64>::layout(len).size() % LINE, 0);
            assert!(Lines::<u64>::layout(len).size() >= len * 8);
            assert!(table.iter().copied().eq(0..len as u64));
        }
    }

    #[test]
    fn the_items_are_dropped_once() {
        let item = std::rc::Rc::new(());
        let mut table = Lines::from(vec![item.clone(), item.clone(), item.clone()]);
        assert_eq!(std::rc::Rc::strong_count(&item), 4);
        table[0] = item.clone();
        drop(table);
        assert_eq!(std::rc::Rc::strong_count(&item), 1);
    }
}
