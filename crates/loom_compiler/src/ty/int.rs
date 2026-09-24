//! The integer oracle: the one place that knows each integer type's range
//! and what each overflow policy does. The type checker uses it to check
//! literals and fold constants; tests compare the Lua runtime against it.
//!
//! Values are carried as `i128`, which holds every value of every
//! supported type (up to `u64`/`i64`; wider types are rejected in v1).

use super::IntTy;

impl IntTy {
    pub fn min(self) -> i128 {
        if self.signed { -(1i128 << (self.bits - 1)) } else { 0 }
    }

    pub fn max(self) -> i128 {
        if self.signed { (1i128 << (self.bits - 1)) - 1 } else { (1i128 << self.bits) - 1 }
    }

    pub fn contains(self, v: i128) -> bool {
        self.min() <= v && v <= self.max()
    }

    /// Whether every value of `self` is a value of `other`: the only implicit
    /// numeric conversion Loom has.
    pub fn widens_to(self, other: IntTy) -> bool {
        other.min() <= self.min() && self.max() <= other.max()
    }

    /// Two's-complement wrap into range: `+%`, `as%`.
    pub fn wrap(self, v: i128) -> i128 {
        let modulus = 1i128 << self.bits;
        let mut r = v.rem_euclid(modulus);
        if self.signed && r > self.max() {
            r -= modulus;
        }
        r
    }

    /// Clamp into range: `+|`, `as|`.
    pub fn saturate(self, v: i128) -> i128 {
        v.clamp(self.min(), self.max())
    }

    /// `Some(v)` if it fits: `+?`, `as?`, and plain arithmetic (which
    /// unravels on `None`).
    pub fn checked(self, v: i128) -> Option<i128> {
        self.contains(v).then_some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::super::IntTy;

    fn int(signed: bool, bits: u8) -> IntTy {
        IntTy { signed, bits, size: false }
    }

    #[test]
    fn ranges() {
        assert_eq!((int(false, 8).min(), int(false, 8).max()), (0, 255));
        assert_eq!((int(true, 8).min(), int(true, 8).max()), (-128, 127));
        assert_eq!((int(false, 1).min(), int(false, 1).max()), (0, 1));
        assert_eq!((int(true, 1).min(), int(true, 1).max()), (-1, 0));
        assert_eq!(int(false, 12).max(), 4095);
        assert_eq!(int(true, 64).min(), i64::MIN as i128);
        assert_eq!(int(false, 64).max(), u64::MAX as i128);
    }

    #[test]
    fn overflow_policies_match_the_docs() {
        // The table in 01-syntax: `200u8 + 100`.
        let u8_ = int(false, 8);
        assert_eq!(u8_.wrap(300), 44);
        assert_eq!(u8_.saturate(300), 255);
        assert_eq!(u8_.checked(300), None);
        assert_eq!(u8_.wrap(-1), 255);
        let i8_ = int(true, 8);
        assert_eq!(i8_.wrap(128), -128);
        assert_eq!(i8_.saturate(-500), -128);
        assert_eq!(int(false, 4).wrap(16), 0);
    }

    #[test]
    fn widening_is_range_inclusion() {
        assert!(int(false, 8).widens_to(int(false, 16)));
        assert!(int(false, 12).widens_to(int(false, 16)));
        assert!(int(false, 8).widens_to(int(true, 16)));
        assert!(!int(false, 16).widens_to(int(true, 16)));
        assert!(!int(true, 8).widens_to(int(false, 64)));
        assert!(int(true, 8).widens_to(int(true, 8)));
    }
}
