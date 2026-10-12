//! A task's value stack: 8-byte slots in segments that never move
//! (design/vm.md §9.2).
//!
//! Frames sit one after another in a segment. When a call's frame doesn't
//! fit in what's left, the frame starts a new segment (and the interpreter
//! copies its arguments there). Segments never move, so an address inside a frame stays
//! good for as long as the frame lives. A segment given back is kept for the
//! next time, so a call right at the edge in a loop doesn't allocate each
//! time.

use crate::Trap;

/// Slots in a segment, unless a frame needs more.
const SEGMENT: usize = 1 << 17;

#[derive(Debug)]
pub(crate) struct Stack {
    segments: Vec<Box<[u64]>>,
    /// The segment in use.
    current: usize,
    /// The bytes of every segment, and how many there may be.
    bytes: usize,
    limit: usize,
    /// How many slots the first segment holds, at least.
    first: usize,
}

impl Stack {
    /// A stack of at most `limit` bytes whose first segment holds `need`
    /// slots.
    pub fn new(limit: usize, need: usize) -> Result<Stack, Trap> {
        Stack::task(limit, need, SEGMENT)
    }

    /// A thread's stack: a first segment of `first` slots (or `need`, if
    /// that's more), so starting one is cheap. Later ones are the usual size.
    pub fn task(limit: usize, need: usize, first: usize) -> Result<Stack, Trap> {
        let mut stack = Stack { segments: Vec::new(), current: 0, bytes: 0, limit, first };
        stack.allocate(need)?;
        Ok(stack)
    }

    /// No segments at all: what a task's stack is while another's is in use.
    pub fn empty() -> Stack {
        Stack { segments: Vec::new(), current: 0, bytes: 0, limit: 0, first: SEGMENT }
    }

    fn allocate(&mut self, need: usize) -> Result<(), Trap> {
        let usual = if self.segments.is_empty() { self.first } else { SEGMENT };
        let slots = need.max(usual.min(self.limit / 8));
        let bytes = slots.checked_mul(8).ok_or(Trap::StackOverflow)?;
        if self.bytes + bytes > self.limit {
            return Err(Trap::StackOverflow);
        }
        self.bytes += bytes;
        self.segments.push(vec![0; slots].into_boxed_slice());
        Ok(())
    }

    /// Whether the first segment holds `need` slots.
    pub fn first_holds(&self, need: usize) -> bool {
        self.segments[0].len() >= need
    }

    /// Back to the first segment, for a new call from outside.
    pub fn reset(&mut self) {
        self.current = 0;
    }

    /// The first and one-past-the-last slot of the segment in use.
    pub fn bounds(&mut self) -> (*mut u64, *mut u64) {
        let range = self.segments[self.current].as_mut_ptr_range();
        (range.start, range.end)
    }

    /// Moves to the next segment, of at least `need` slots, and gives its
    /// bounds.
    pub fn grow(&mut self, need: usize) -> Result<(*mut u64, *mut u64), Trap> {
        let next = self.current + 1;
        if next < self.segments.len() && self.segments[next].len() < need {
            // Too small for this frame: drop it (and those kept after it)
            // to make room for a bigger one.
            for old in self.segments.drain(next..) {
                self.bytes -= old.len() * 8;
            }
        }
        if next >= self.segments.len() {
            self.allocate(need)?;
        }
        self.current = next;
        Ok(self.bounds())
    }

    /// Goes back to the previous segment, and gives its end.
    pub fn shrink(&mut self) -> *mut u64 {
        self.current -= 1;
        self.bounds().1
    }

    /// The address range of each segment in use.
    pub fn ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.segments[..self.segments.len().min(self.current + 1)].iter().map(|s| {
            let r = s.as_ptr_range();
            (r.start as u64, r.end as u64)
        })
    }

    /// Whether `len` bytes at `addr` are inside a segment in use.
    pub fn holds(&self, addr: u64, len: u64) -> bool {
        self.segments[..=self.current].iter().any(|s| {
            let r = s.as_ptr_range();
            addr >= r.start as u64 && addr.checked_add(len).is_some_and(|end| end <= r.end as u64)
        })
    }
}
