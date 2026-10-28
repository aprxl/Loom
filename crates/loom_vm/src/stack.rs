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

/// With `checked`: every segment of every stack, by address with its end,
/// so that a thread's access to its parent's frame (on a stack another
/// worker is running) is known to be in a stack.
#[cfg(feature = "checked")]
static SEGMENTS: std::sync::Mutex<std::collections::BTreeMap<u64, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// With `checked`: whether `len` bytes at `addr` are inside a segment of
/// some stack.
#[cfg(feature = "checked")]
pub(crate) fn in_some_stack(addr: u64, len: u64) -> bool {
    let all = SEGMENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match all.range(..=addr).next_back() {
        Some((_, &end)) => addr.checked_add(len).is_some_and(|stop| stop <= end),
        None => false,
    }
}

#[cfg(not(feature = "checked"))]
pub(crate) fn in_some_stack(_: u64, _: u64) -> bool {
    false
}

#[cfg(feature = "checked")]
fn register(segment: &[u64]) {
    let r = segment.as_ptr_range();
    SEGMENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(r.start as u64, r.end as u64);
}

#[cfg(feature = "checked")]
fn unregister(segment: &[u64]) {
    SEGMENTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&(segment.as_ptr() as u64));
}

#[cfg(not(feature = "checked"))]
fn register(_: &[u64]) {}

#[cfg(not(feature = "checked"))]
fn unregister(_: &[u64]) {}

impl Drop for Stack {
    fn drop(&mut self) {
        for segment in &self.segments {
            unregister(segment);
        }
    }
}

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
        let segment = vec![0; slots].into_boxed_slice();
        register(&segment);
        self.segments.push(segment);
        Ok(())
    }

    /// Gives back every segment but the first, and starts again in it: a
    /// stack that served a finished task, ready for another. `false` (and
    /// the stack is left alone) when it has no first segment, or one of more
    /// than `slots` slots.
    pub fn recycle(&mut self, slots: usize) -> bool {
        match self.segments.first() {
            Some(first) if first.len() <= slots => {}
            _ => return false,
        }
        for old in self.segments.drain(1..) {
            unregister(&old);
        }
        self.bytes = self.segments[0].len() * 8;
        self.current = 0;
        true
    }

    /// Whether this stack, from `recycle`, does for a task that needs `need`
    /// slots in its first segment, within `limit` bytes.
    pub fn serves(&self, limit: usize, need: usize) -> bool {
        self.limit == limit && self.first_holds(need)
    }

    /// Whether the first segment holds `need` slots.
    pub fn first_holds(&self, need: usize) -> bool {
        self.segments.first().is_some_and(|segment| segment.len() >= need)
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
                unregister(&old);
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

    /// Whether `len` bytes at `addr` are inside a segment in use.
    pub fn holds(&self, addr: u64, len: u64) -> bool {
        self.segments[..=self.current].iter().any(|s| {
            let r = s.as_ptr_range();
            addr >= r.start as u64 && addr.checked_add(len).is_some_and(|end| end <= r.end as u64)
        })
    }
}
