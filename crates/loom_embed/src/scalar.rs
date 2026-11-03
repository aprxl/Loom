//! The Rust types that cross into Loom: a value is one register.

/// A Rust type with a Loom twin of one register: signed integers are
/// sign-extended and unsigned ones zero-extended, as Treadle keeps them.
pub trait Scalar: Copy + 'static {
    /// The Loom type's name.
    const LOOM: &'static str;
    fn to_reg(self) -> u64;
    fn from_reg(reg: u64) -> Self;
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
}

impl Scalar for char {
    const LOOM: &'static str = "char";
    fn to_reg(self) -> u64 {
        u64::from(self)
    }
    fn from_reg(reg: u64) -> Self {
        u32::try_from(reg).ok().and_then(char::from_u32).unwrap_or(char::REPLACEMENT_CHARACTER)
    }
}

/// The type names a raw registration may use.
pub(crate) const TYPES: [&str; 10] = ["i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "bool", "char"];

/// `value` written as a Loom literal of its type.
pub(crate) fn literal<T: Scalar>(value: T) -> String {
    let reg = value.to_reg();
    match T::LOOM {
        "bool" => (reg != 0).to_string(),
        "char" => format!("'{}'", char::from_reg(reg).escape_default()),
        t if t.starts_with('i') => (reg as i64).to_string(),
        _ => reg.to_string(),
    }
}
