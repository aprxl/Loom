//! The Rust types that cross into Loom: a value is one register.

use crate::HostCx;

/// A Rust type with a Loom twin of one register: signed integers are
/// sign-extended, unsigned ones zero-extended, and floats are their bits, as
/// Treadle keeps them.
pub trait Scalar: Copy + 'static {
    /// The Loom type's name.
    const LOOM: &'static str;
    fn to_reg(self) -> u64;
    fn from_reg(reg: u64) -> Self;
    /// `self` written as a Loom literal of its type, or `None` for a float
    /// with no literal (NaN, infinity).
    fn literal(self) -> Option<String>;
}

macro_rules! signed {
    ($($t:ident),*) => {$(
        impl Scalar for $t {
            const LOOM: &'static str = stringify!($t);
            fn to_reg(self) -> u64 {
                self as i64 as u64
            }
            fn from_reg(reg: u64) -> Self {
                reg as i64 as $t
            }
            fn literal(self) -> Option<String> {
                Some(self.to_string())
            }
        }
    )*};
}

macro_rules! unsigned {
    ($($t:ident),*) => {$(
        impl Scalar for $t {
            const LOOM: &'static str = stringify!($t);
            fn to_reg(self) -> u64 {
                self as u64
            }
            fn from_reg(reg: u64) -> Self {
                reg as $t
            }
            fn literal(self) -> Option<String> {
                Some(self.to_string())
            }
        }
    )*};
}

signed!(i8, i16, i32, i64);
unsigned!(u8, u16, u32, u64);

impl Scalar for bool {
    const LOOM: &'static str = "bool";
    fn to_reg(self) -> u64 {
        u64::from(self)
    }
    fn from_reg(reg: u64) -> Self {
        reg != 0
    }
    fn literal(self) -> Option<String> {
        Some(self.to_string())
    }
}

impl Scalar for char {
    const LOOM: &'static str = "char";
    fn to_reg(self) -> u64 {
        u64::from(self)
    }
    fn from_reg(reg: u64) -> Self {
        u32::try_from(reg).ok().and_then(char::from_u32).unwrap_or(char::REPLACEMENT_CHARACTER)
    }
    fn literal(self) -> Option<String> {
        Some(format!("'{}'", self.escape_default()))
    }
}

impl Scalar for f32 {
    const LOOM: &'static str = "f32";
    fn to_reg(self) -> u64 {
        u64::from(self.to_bits())
    }
    fn from_reg(reg: u64) -> Self {
        f32::from_bits(reg as u32)
    }
    fn literal(self) -> Option<String> {
        self.is_finite().then(|| format!("{self:?}"))
    }
}

impl Scalar for f64 {
    const LOOM: &'static str = "f64";
    fn to_reg(self) -> u64 {
        self.to_bits()
    }
    fn from_reg(reg: u64) -> Self {
        f64::from_bits(reg)
    }
    fn literal(self) -> Option<String> {
        self.is_finite().then(|| format!("{self:?}"))
    }
}

/// What a host function gives back: one register for a scalar, or for a
/// `String` a slot that the host fills, passed as the first register.
pub trait Out: Sized {
    /// The Loom type's name.
    const LOOM: &'static str;
    /// The registers before the arguments: a `String`'s slot.
    const LEAD: usize;
    /// Writes the value: a scalar to `r[0]`, a `String` into the slot `r[0]`.
    fn put(self, cx: &mut HostCx<'_>, r: &mut [u64]) -> Result<(), String>;
}

impl<T: Scalar> Out for T {
    const LOOM: &'static str = T::LOOM;
    const LEAD: usize = 0;
    fn put(self, _cx: &mut HostCx<'_>, r: &mut [u64]) -> Result<(), String> {
        r[0] = self.to_reg();
        Ok(())
    }
}

impl Out for String {
    const LOOM: &'static str = "String";
    const LEAD: usize = 1;
    fn put(self, cx: &mut HostCx<'_>, r: &mut [u64]) -> Result<(), String> {
        cx.new_string(self.as_bytes(), r[0])
    }
}

/// The type names a registration may use for a scalar.
pub(crate) const TYPES: [&str; 12] =
    ["i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "bool", "char", "f32", "f64"];
