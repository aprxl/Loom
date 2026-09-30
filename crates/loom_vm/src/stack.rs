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
}

impl Stack {
    /// A stack of at most `limit` bytes whose first segment holds `need`
    /// slots.
    pub fn new(limit: usize, need: usize) -> Result<Stack, Trap> {
        let mut stack = Stack { segments: Vec::new(), current: 0, bytes: 0, limit };
        stack.allocate(need)?;
        Ok(stack)
    }

    fn allocate(&mut self, need: usize) -> Result<(), Trap> {
        let slots = need.max(SEGMENT.min(self.limit / 8));
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
}
