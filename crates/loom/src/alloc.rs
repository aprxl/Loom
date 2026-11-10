//! The allocator of the `loom` binary.
//!
//! Compiling makes tens of thousands of small objects that live a moment
//! (names, syntax nodes, lists of a few items), and the system's allocator
//! took a third of a compile handing them out and taking them back. This one
//! works as Treadle's heap does (`loom_vm::heap`): blocks of up to [`SMALL`]
//! bytes, aligned to at most [`STEP`], come in size classes [`STEP`] bytes
//! apart. Each thread has a free list per class and a chunk it carves new
//! blocks from, so allocating and freeing take no lock; a block's class
//! follows from its size, which `dealloc` is given, so blocks have no header.
//! Bigger blocks go to the system's allocator.
//!
//! A block may be freed on another thread than the one that made it, and it
//! goes on the freeing thread's list. So that a thread that frees more than
//! it allocates doesn't keep it all, a list that reaches a batch (a chunk's
//! worth of blocks) is set aside, and the one set aside before it goes to a
//! pool every thread shares, where a thread with nothing free takes one
//! before it carves. A thread that ends gives the pool what it has. Chunks
//! are never given back to the system: their blocks are used again.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, UnsafeCell};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};

/// The biggest block a size class holds.
const SMALL: usize = 1024;
/// How far apart the classes' sizes are, and the alignment of their blocks.
const STEP: usize = 16;
const CLASSES: usize = SMALL / STEP;
/// The size of the chunks blocks are carved from.
const CHUNK: usize = 64 * 1024;

const _: () = assert!(STEP >= 2 * size_of::<usize>(), "a free block holds two links");

/// The class of blocks of `size` bytes (`0 < size <= SMALL`).
fn class_of(size: usize) -> usize {
    (size - 1) / STEP
}

/// The size of a block of `class`.
fn size_of_class(class: usize) -> usize {
    (class + 1) * STEP
}

/// How many blocks of `class` make a batch: a chunk's worth.
fn batch(class: usize) -> u32 {
    (CHUNK / size_of_class(class)) as u32
}

/// Whether blocks of `layout` come from the size classes.
fn is_small(layout: Layout) -> bool {
    layout.size() <= SMALL && layout.align() <= STEP
}

/// The block after `block` in its list.
///
/// # Safety
///
/// `block` is a free block of a size class.
unsafe fn next(block: *mut u8) -> *mut u8 {
    // SAFETY: a free block's first word is its link, and blocks are aligned
    // to `STEP`.
    unsafe { *block.cast::<*mut u8>() }
}

/// Makes `block`'s link `to`.
///
/// # Safety
///
/// `block` is a block of a size class that nothing else uses.
unsafe fn set_next(block: *mut u8, to: *mut u8) {
    // SAFETY: as for `next`.
    unsafe { *block.cast::<*mut u8>() = to }
}

/// A free block's second word: in the pool, a list's first block says where
/// the next list is, and a chunk's unused end where it ends.
///
/// # Safety
///
/// `block` is a block of a size class (or an unused end of a chunk, of at
/// least `STEP` bytes) that nothing else uses.
unsafe fn second(block: *mut u8) -> *mut *mut u8 {
    // SAFETY: every block is at least two words.
    unsafe { block.cast::<*mut u8>().add(1) }
}

/// A thread's blocks.
struct Local {
    /// Each class's free blocks, as a list through the blocks themselves.
    free: [Cell<*mut u8>; CLASSES],
    /// How many blocks each list of `free` has: fewer than a batch.
    count: [Cell<u32>; CLASSES],
    /// For each class, a list of exactly a batch set aside, or none.
    spare: [Cell<*mut u8>; CLASSES],
    /// Where the chunk being carved from is free, to its end.
    next: Cell<usize>,
    end: Cell<usize>,
    /// Whether [`Exit`] is set to see this thread end.
    watched: Cell<bool>,
}

thread_local! {
    // No destructor, so the allocator works for as long as the thread runs
    // anything (other thread-locals' destructors too).
    static LOCAL: Local = const {
        Local {
            free: [const { Cell::new(null_mut()) }; CLASSES],
            count: [const { Cell::new(0) }; CLASSES],
            spare: [const { Cell::new(null_mut()) }; CLASSES],
            next: Cell::new(0),
            end: Cell::new(0),
            watched: Cell::new(false),
        }
    };
    static EXIT: Exit = const { Exit };
}

/// Gives a thread's blocks to the pool when the thread ends.
struct Exit;

impl Drop for Exit {
    fn drop(&mut self) {
        LOCAL.with(Local::give_up);
    }
}

impl Local {
    fn alloc(&self, class: usize) -> *mut u8 {
        let block = self.free[class].get();
        if block.is_null() {
            return self.refill(class);
        }
        // SAFETY: a block of a free list.
        self.free[class].set(unsafe { next(block) });
        self.count[class].set(self.count[class].get() - 1);
        block
    }

    /// # Safety
    ///
    /// `block` is a block of `class` that nothing uses any more.
    unsafe fn free(&self, block: *mut u8, class: usize) {
        if !self.watched.get() {
            self.watch();
        }
        // SAFETY: the caller's.
        unsafe { set_next(block, self.free[class].get()) };
        self.free[class].set(block);
        let count = self.count[class].get() + 1;
        self.count[class].set(count);
        if count == batch(class) {
            self.set_aside(class);
        }
    }

    /// Has [`Exit`] give this thread's blocks to the pool when it ends: before
    /// the thread has any, and with nothing half done, since registering the
    /// destructor may allocate.
    #[cold]
    fn watch(&self) {
        self.watched.set(true);
        let _ = EXIT.try_with(|_| {});
    }

    /// The free list of `class` is a batch: it's set aside, and the one set
    /// aside before it goes to the pool.
    #[cold]
    fn set_aside(&self, class: usize) {
        let full = self.free[class].replace(null_mut());
        self.count[class].set(0);
        let spare = self.spare[class].replace(full);
        if !spare.is_null() {
            // SAFETY: a list set aside is a batch of free blocks of `class`.
            unsafe { push(&mut POOL.lock().full[class], spare) };
        }
    }

    /// A block of `class`, whose free list is empty: from the list set
    /// aside, or the pool's, or carved from a chunk. Null when the system
    /// has no memory for a chunk.
    #[cold]
    fn refill(&self, class: usize) -> *mut u8 {
        if !self.watched.get() {
            self.watch();
            return self.alloc(class);
        }
        let mut count = batch(class);
        let mut list = self.spare[class].replace(null_mut());
        if list.is_null() {
            list = pop(&mut POOL.lock().full[class]);
        }
        if list.is_null() {
            list = pop(&mut POOL.lock().short[class]);
            if list.is_null() {
                return self.carve(class);
            }
            // SAFETY: a list of free blocks.
            count = unsafe { length(list) };
        }
        // SAFETY: a list of `count` free blocks: its first is the one given.
        self.free[class].set(unsafe { next(list) });
        self.count[class].set(count - 1);
        list
    }

    fn carve(&self, class: usize) -> *mut u8 {
        let size = size_of_class(class);
        while self.end.get() - self.next.get() < size {
            if !self.next_chunk() {
                return null_mut();
            }
        }
        let block = self.next.get();
        self.next.set(block + size);
        block as *mut u8
    }

    /// Goes on to carve from the unused end of a chunk a thread left, or
    /// from a new chunk; what's left of the old one becomes a free block.
    /// False when the system has no memory for a chunk.
    fn next_chunk(&self) -> bool {
        let (mut start, mut end) = POOL.lock().take_end();
        if start == 0 {
            // SAFETY: the layout has a size.
            start = unsafe { System.alloc(Layout::from_size_align_unchecked(CHUNK, STEP)) } as usize;
            if start == 0 {
                return false;
            }
            end = start + CHUNK;
        }
        let rest = self.next.replace(start);
        let rest_len = self.end.replace(end) - rest;
        if rest_len != 0 {
            // SAFETY: `rest_len` bytes at `rest` (a multiple of `STEP`, at
            // most `SMALL`) are no block's.
            unsafe { self.free(rest as *mut u8, class_of(rest_len)) };
        }
        true
    }

    /// The thread ends: the pool gets its lists and the rest of its chunk.
    fn give_up(&self) {
        let mut pool = POOL.lock();
        for class in 0..CLASSES {
            let spare = self.spare[class].replace(null_mut());
            if !spare.is_null() {
                // SAFETY: a list set aside is a batch of free blocks.
                unsafe { push(&mut pool.full[class], spare) };
            }
            let free = self.free[class].replace(null_mut());
            self.count[class].set(0);
            if !free.is_null() {
                // SAFETY: a list of free blocks.
                unsafe { push(&mut pool.short[class], free) };
            }
        }
        let (start, end) = (self.next.replace(0), self.end.replace(0));
        if end - start >= STEP {
            // SAFETY: the rest of this thread's chunk is no block's.
            unsafe { pool.give_end(start, end) };
        }
    }
}

/// How many blocks `list` has.
///
/// # Safety
///
/// `list` is a list of free blocks.
unsafe fn length(mut list: *mut u8) -> u32 {
    let mut count = 0;
    while !list.is_null() {
        count += 1;
        // SAFETY: a block of the list.
        list = unsafe { next(list) };
    }
    count
}

/// Puts `list` on `stack`: a list of lists, linked by the second word of each
/// list's first block.
///
/// # Safety
///
/// `list` is a list of free blocks, not empty.
unsafe fn push(stack: &mut *mut u8, list: *mut u8) {
    // SAFETY: the list's first block is free.
    unsafe { *second(list) = *stack };
    *stack = list;
}

/// Takes a list off `stack`, or gives null.
fn pop(stack: &mut *mut u8) -> *mut u8 {
    let list = *stack;
    if !list.is_null() {
        // SAFETY: the first block of a list `push` put here.
        *stack = unsafe { *second(list) };
    }
    list
}

/// What the threads share: the lists and ends of chunks they gave up.
struct Pool {
    /// Each class's batches, as `push` stacks them.
    full: [*mut u8; CLASSES],
    /// Each class's lists short of a batch, from threads that ended.
    short: [*mut u8; CLASSES],
    /// The unused ends of the chunks of threads that ended: each starts with
    /// the next one's address and its own end.
    ends: *mut u8,
}

impl Pool {
    /// # Safety
    ///
    /// `start..end` is at least `STEP` bytes of a chunk that are no block's.
    unsafe fn give_end(&mut self, start: usize, end: usize) {
        let block = start as *mut u8;
        // SAFETY: the caller's.
        unsafe {
            set_next(block, self.ends);
            *second(block) = end as *mut u8;
        }
        self.ends = block;
    }

    /// An unused end of a chunk, as where it starts and ends, or zeros.
    fn take_end(&mut self) -> (usize, usize) {
        let block = self.ends;
        if block.is_null() {
            return (0, 0);
        }
        // SAFETY: `give_end` wrote both words.
        unsafe {
            self.ends = next(block);
            (block as usize, *second(block) as usize)
        }
    }
}

/// The pool behind a spin lock: it's held for a few moves of pointers, once
/// per batch, and nothing done under it allocates or waits.
struct Shared {
    locked: AtomicBool,
    pool: UnsafeCell<Pool>,
}

// SAFETY: the pool is only reached through `lock`, by one thread at a time.
unsafe impl Sync for Shared {}

static POOL: Shared = Shared {
    locked: AtomicBool::new(false),
    pool: UnsafeCell::new(Pool { full: [null_mut(); CLASSES], short: [null_mut(); CLASSES], ends: null_mut() }),
};

struct Held<'s>(&'s Shared);

impl Shared {
    fn lock(&self) -> Held<'_> {
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
        Held(self)
    }
}

impl std::ops::Deref for Held<'_> {
    type Target = Pool;

    fn deref(&self) -> &Pool {
        // SAFETY: the lock is held.
        unsafe { &*self.0.pool.get() }
    }
}

impl std::ops::DerefMut for Held<'_> {
    fn deref_mut(&mut self) -> &mut Pool {
        // SAFETY: the lock is held.
        unsafe { &mut *self.0.pool.get() }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::Release);
    }
}

/// The allocator: see the module's documentation.
#[derive(Debug)]
pub struct Classes;

// SAFETY: a small block is `size_of_class` bytes of a chunk, aligned to
// `STEP`, that no other live block overlaps: it's carved once, and handed out
// again only after `dealloc` put it on a list. The rest is the system's.
unsafe impl GlobalAlloc for Classes {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if is_small(layout) {
            LOCAL.with(|local| local.alloc(class_of(layout.size())))
        } else {
            // SAFETY: the caller's.
            unsafe { System.alloc(layout) }
        }
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        if is_small(layout) {
            // SAFETY: `alloc` gave a block of this class for this layout.
            LOCAL.with(|local| unsafe { local.free(block, class_of(layout.size())) });
        } else {
            // SAFETY: the caller's.
            unsafe { System.dealloc(block, layout) }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !is_small(layout) {
            // SAFETY: the caller's.
            return unsafe { System.alloc_zeroed(layout) };
        }
        // SAFETY: the caller's.
        let block = unsafe { self.alloc(layout) };
        if !block.is_null() {
            // SAFETY: `layout.size()` bytes just allocated.
            unsafe { block.write_bytes(0, layout.size()) };
        }
        block
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller made sure of the new size and this alignment.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        match (is_small(layout), is_small(new_layout)) {
            // SAFETY: the caller's.
            (false, false) => unsafe { System.realloc(block, layout, new_size) },
            (true, true) if class_of(layout.size()) == class_of(new_size) => block,
            // SAFETY: the caller's, and the blocks don't overlap.
            _ => unsafe {
                let new = self.alloc(new_layout);
                if !new.is_null() {
                    new.copy_from_nonoverlapping(block, layout.size().min(new_size));
                    self.dealloc(block, layout);
                }
                new
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 8).expect("a valid layout")
    }

    /// Allocates a block of each of `sizes` and fills it with `tag`.
    fn fill(sizes: impl Iterator<Item = usize>, tag: u8) -> Vec<(usize, usize)> {
        sizes
            .map(|size| {
                // SAFETY: the layout has a size.
                let block = unsafe { Classes.alloc(layout(size)) };
                assert!(!block.is_null() && (block as usize).is_multiple_of(STEP));
                // SAFETY: just allocated.
                unsafe { block.write_bytes(tag, size) };
                (block as usize, size)
            })
            .collect()
    }

    /// Checks that each block still holds `tag`, and frees it.
    fn drain(blocks: Vec<(usize, usize)>, tag: u8) {
        for (block, size) in blocks {
            // SAFETY: `fill` allocated and filled it.
            unsafe {
                assert!(std::slice::from_raw_parts(block as *const u8, size).iter().all(|&b| b == tag));
                Classes.dealloc(block as *mut u8, layout(size));
            }
        }
    }

    #[test]
    fn classes_cover_every_small_size() {
        assert_eq!((class_of(1), class_of(16), class_of(17), class_of(SMALL)), (0, 0, 1, CLASSES - 1));
        for size in 1..=SMALL {
            let room = size_of_class(class_of(size));
            assert!((size..size + STEP).contains(&room), "{size} in {room}");
        }
    }

    #[test]
    fn live_blocks_never_overlap() {
        let sizes = || (1..=3000).map(|i| (i * 37) % (SMALL + 300) + 1);
        let mut blocks = fill(sizes(), 0xA5);
        // Free every other one and allocate again: what's reused must be only what was freed.
        let (keep, free): (Vec<_>, Vec<_>) = blocks.drain(..).enumerate().partition(|(i, _)| i % 2 == 0);
        drain(free.into_iter().map(|(_, b)| b).collect(), 0xA5);
        let again = fill(sizes(), 0x5A);
        let mut all: Vec<(usize, usize)> = keep.iter().map(|&(_, b)| b).chain(again.iter().copied()).collect();
        all.sort_unstable();
        assert!(all.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0), "two live blocks overlap");
        drain(keep.into_iter().map(|(_, b)| b).collect(), 0xA5);
        drain(again, 0x5A);
    }

    #[test]
    fn realloc_keeps_the_bytes() {
        for (from, to) in [(8, 16), (16, 17), (24, 2000), (2000, 24), (2000, 5000), (700, 100)] {
            // SAFETY: allocated here with these layouts, and written inside them.
            unsafe {
                let block = Classes.alloc(layout(from));
                for i in 0..from {
                    block.add(i).write(i as u8);
                }
                let block = Classes.realloc(block, layout(from), to);
                assert!((0..from.min(to)).all(|i| block.add(i).read() == i as u8), "{from} to {to}");
                Classes.dealloc(block, layout(to));
            }
        }
        // SAFETY: allocated here.
        unsafe {
            let block = Classes.alloc_zeroed(layout(300));
            assert!(std::slice::from_raw_parts(block, 300).iter().all(|&b| b == 0));
            Classes.dealloc(block, layout(300));
        }
    }

    #[test]
    fn blocks_cross_threads_and_come_back() {
        // One thread allocates and ends, another frees and ends, over and
        // over: what's freed must reach the next thread through the pool, so
        // after the first rounds a round's blocks are ones seen before. (Not
        // all: other tests' threads take from the pool too.)
        let mut seen = HashSet::new();
        let mut round = || {
            let blocks = std::thread::spawn(|| fill((0..20_000).map(|i| i % 200 + 1), 7)).join().expect("no panic");
            let old = blocks.iter().filter(|&&(block, _)| !seen.insert(block)).count();
            std::thread::spawn(move || drain(blocks, 7)).join().expect("no panic");
            old
        };
        (0..10).for_each(|_| _ = round());
        let old: usize = (0..30).map(|_| round()).sum();
        assert!(old > 30 * 20_000 / 2, "only {old} of 600000 blocks were used before");
    }
}
