//! Arenas (design/v6-spools.md): a spool that allocates by bumping a pointer
//! and frees everything at once.
//!
//! An arena is a header and a chain of chunks, all taken from the worker's
//! [`Heap`] (so the checked build sees them like any object). The header is
//! five words:
//!
//! | word | |
//! |---|---|
//! | 0 | where the next object goes |
//! | 1 | where the current chunk ends |
//! | 2 | the current chunk |
//! | 3 | the first chunk |
//! | 4 | the size of the first chunk's data, which the next ones double |
//!
//! A chunk is a link to the next one, the size of its data, and the data
//! (16-aligned). Every object is rounded up to 16 bytes, so every address is
//! 16-aligned and allocating is a compare and an add. `reset` goes back to
//! the first chunk and frees the rest.

#![allow(unsafe_code)]

use crate::Trap;
use crate::heap::Heap;

const HEADER: u64 = 40;
const CHUNK_HEADER: u64 = 16;
/// The smallest first chunk.
const MIN_CHUNK: u64 = 4096;

const CUR: usize = 0;
const END: usize = 1;
const CHUNK: usize = 2;
const FIRST: usize = 3;
const SIZE: usize = 4;

fn words(header: u64) -> *mut u64 {
    header as *mut u64
}

fn round(n: u64) -> Result<u64, Trap> {
    n.checked_add(15).map(|m| m & !15).ok_or(Trap::OutOfMemory)
}

/// A chunk of `data` bytes of data, linked to nothing.
fn new_chunk(heap: &mut Heap, data: u64) -> Result<u64, Trap> {
    let size = CHUNK_HEADER.checked_add(data).ok_or(Trap::OutOfMemory)?;
    let chunk = heap.alloc(size, 16)?;
    // SAFETY: `chunk` is a new object of `CHUNK_HEADER + data` bytes.
    unsafe {
        let w = words(chunk);
        w.write_unaligned(0);
        w.add(1).write_unaligned(data);
    }
    Ok(chunk)
}

/// `arena.new(capacity) -> arena`: an arena whose first chunk holds
/// `capacity` bytes.
pub(crate) fn new(heap: &mut Heap, capacity: u64) -> Result<u64, Trap> {
    let data = round(capacity.max(MIN_CHUNK))?;
    let header = heap.alloc(HEADER, 16)?;
    let chunk = match new_chunk(heap, data) {
        Ok(chunk) => chunk,
        Err(e) => {
            heap.free(header, HEADER, 16)?;
            return Err(e);
        }
    };
    // SAFETY: `header` is a new object of `HEADER` bytes and `chunk` one
    // with its data after `CHUNK_HEADER`.
    unsafe {
        let w = words(header);
        w.add(CUR).write_unaligned(chunk + CHUNK_HEADER);
        w.add(END).write_unaligned(chunk + CHUNK_HEADER + data);
        w.add(CHUNK).write_unaligned(chunk);
        w.add(FIRST).write_unaligned(chunk);
        w.add(SIZE).write_unaligned(data);
    }
    Ok(header)
}

/// `arena.alloc(arena, size) -> address`: `size` bytes, 16-aligned.
pub(crate) fn alloc(heap: &mut Heap, header: u64, size: u64) -> Result<u64, Trap> {
    let size = round(size.max(1))?;
    // SAFETY: `header` is an arena `new` made and `free` hasn't freed (the
    // compiler drops an arena once, at the end of its scope); its words are
    // the state described above.
    unsafe {
        let w = words(header);
        let cur = w.add(CUR).read_unaligned();
        let end = w.add(END).read_unaligned();
        if end - cur >= size {
            w.add(CUR).write_unaligned(cur + size);
            return Ok(cur);
        }
        grow(heap, w, size)
    }
}

/// The slow path of [`alloc`]: a new chunk, at least twice the last one's.
///
/// # Safety
///
/// `w` is the header of a live arena.
#[cold]
#[inline(never)]
unsafe fn grow(heap: &mut Heap, w: *mut u64, size: u64) -> Result<u64, Trap> {
    // SAFETY: as the caller says; the chunks are the ones this arena made.
    unsafe {
        let last = w.add(CHUNK).read_unaligned();
        let last_size = (last as *const u64).add(1).read_unaligned();
        let doubled = last_size.checked_mul(2).ok_or(Trap::OutOfMemory)?;
        let data = round(doubled.max(size))?;
        let chunk = new_chunk(heap, data)?;
        (last as *mut u64).write_unaligned(chunk);
        w.add(CHUNK).write_unaligned(chunk);
        w.add(END).write_unaligned(chunk + CHUNK_HEADER + data);
        w.add(CUR).write_unaligned(chunk + CHUNK_HEADER + size);
        Ok(chunk + CHUNK_HEADER)
    }
}

/// `arena.reset(arena)`: everything allocated so far is gone; the first
/// chunk is used again from its start.
pub(crate) fn reset(heap: &mut Heap, header: u64) -> Result<(), Trap> {
    // SAFETY: as for [`alloc`].
    unsafe {
        let w = words(header);
        let first = w.add(FIRST).read_unaligned();
        let mut chunk = (first as *const u64).read_unaligned();
        while chunk != 0 {
            let next = (chunk as *const u64).read_unaligned();
            let data = (chunk as *const u64).add(1).read_unaligned();
            heap.free(chunk, CHUNK_HEADER + data, 16)?;
            chunk = next;
        }
        let data = w.add(SIZE).read_unaligned();
        (first as *mut u64).write_unaligned(0);
        if cfg!(feature = "checked") {
            std::ptr::write_bytes((first + CHUNK_HEADER) as *mut u8, 0xDD, data as usize);
        }
        w.add(CUR).write_unaligned(first + CHUNK_HEADER);
        w.add(END).write_unaligned(first + CHUNK_HEADER + data);
        w.add(CHUNK).write_unaligned(first);
    }
    Ok(())
}

/// `arena.free(arena)`: the arena and its chunks.
pub(crate) fn free(heap: &mut Heap, header: u64) -> Result<(), Trap> {
    // SAFETY: as for [`alloc`].
    unsafe {
        let w = words(header);
        let mut chunk = w.add(FIRST).read_unaligned();
        while chunk != 0 {
            let next = (chunk as *const u64).read_unaligned();
            let data = (chunk as *const u64).add(1).read_unaligned();
            heap.free(chunk, CHUNK_HEADER + data, 16)?;
            chunk = next;
        }
    }
    heap.free(header, HEADER, 16)
}
