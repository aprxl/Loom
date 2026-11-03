//! Typed host functions: a closure's annotated signature picks its impl.
//!
//! A `Scalar` argument is a `Num<T>` marker and a `&str` argument is `Text`,
//! so each combination of argument kinds has one impl, and inference finds it
//! from the closure's parameter types (no turbofish, no projection). The impls
//! come from `free_impl!` and `method_impl!`, one invocation per combination.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use loom_vm::HostCx;

use crate::scalar::{Out, Scalar};
use crate::{Factory, HandleTable, Layout};

/// A `Scalar` argument: one register.
#[doc(hidden)]
pub struct Num<T>(PhantomData<fn() -> T>);

impl<T> std::fmt::Debug for Num<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Num")
    }
}

/// A `&str` argument: an address and a length, two registers.
#[doc(hidden)]
#[derive(Debug)]
pub struct Text;

/// Reads a `&str` argument from its address and length.
fn text_arg<'a>(cx: &'a HostCx<'_>, addr: u64, len: u64) -> Result<&'a str, String> {
    std::str::from_utf8(cx.text(addr, len)?).map_err(|e| e.to_string())
}

const POISONED: &str = "a handle's table is poisoned";

/// A closure a host function is made from.
pub trait IntoHost<M> {
    /// The Loom parameter types, in order.
    #[doc(hidden)]
    fn params() -> Vec<&'static str>;
    /// The Loom result type.
    #[doc(hidden)]
    fn ret() -> &'static str;
    /// A factory of this closure's host function, one per run.
    #[doc(hidden)]
    fn factory(self) -> Factory;
}

/// A closure a handle method is made from: its first parameter is the value
/// the handle's id names.
pub trait IntoMethod<T, M> {
    /// The Loom parameter types after the id, in order.
    #[doc(hidden)]
    fn params() -> Vec<&'static str>;
    /// The Loom result type.
    #[doc(hidden)]
    fn ret() -> &'static str;
    /// A factory of this closure's host function over the handle's table.
    #[doc(hidden)]
    fn factory(self, table: Arc<Mutex<HandleTable<T>>>) -> Factory;
}

/// The marker of an argument: `N A` for a scalar type `A`, `T` for `&str`.
macro_rules! marker {
    (N $a:ident) => {
        Num<$a>
    };
    (T) => {
        Text
    };
}

/// The closure's parameter type of an argument.
macro_rules! argty {
    (N $a:ident) => {
        $a
    };
    (T) => {
        &str
    };
}

/// The Loom type of an argument.
macro_rules! param {
    (N $a:ident) => {
        <$a as Scalar>::LOOM
    };
    (T) => {
        "str"
    };
}

/// Reads an argument from the registers the iterator `$regs` yields.
macro_rules! arg {
    ($regs:ident, $cx:ident, (N $a:ident)) => {
        <$a as Scalar>::from_reg($regs.next().unwrap_or_default())
    };
    ($regs:ident, $cx:ident, (T)) => {
        text_arg(&*$cx, $regs.next().unwrap_or_default(), $regs.next().unwrap_or_default())?
    };
}

/// The impl of `IntoHost` for a closure whose arguments are the given kinds;
/// `[$g...]` are the scalar type parameters among them.
macro_rules! free_impl {
    ([$($g:ident),*] $(($($d:tt)*))+) => {
        impl<F, R $(, $g)*> IntoHost<($(marker!($($d)*),)+)> for F
        where
            F: FnMut($(argty!($($d)*)),+) -> R + Clone + Send + 'static,
            R: Out,
            $($g: Scalar),*
        {
            fn params() -> Vec<&'static str> {
                vec![$(param!($($d)*)),+]
            }

            fn ret() -> &'static str {
                R::LOOM
            }

            fn factory(self) -> Factory {
                Box::new(move |_: &[Layout]| {
                    let mut f = self.clone();
                    Box::new(move |cx: &mut HostCx<'_>, r: &mut [u64]| {
                        let mut regs = r[R::LEAD..].iter().copied();
                        let out = f($(arg!(regs, cx, ($($d)*))),+);
                        out.put(cx, r)
                    })
                })
            }
        }
    };
}

/// The impl of `IntoMethod` for a closure whose arguments after the value are
/// the given kinds.
macro_rules! method_impl {
    ([$($g:ident),*] $(($($d:tt)*))+) => {
        impl<F, T, R $(, $g)*> IntoMethod<T, ($(marker!($($d)*),)+)> for F
        where
            F: FnMut(&mut T, $(argty!($($d)*)),+) -> R + Clone + Send + 'static,
            T: Send + 'static,
            R: Out,
            $($g: Scalar),*
        {
            fn params() -> Vec<&'static str> {
                vec![$(param!($($d)*)),+]
            }

            fn ret() -> &'static str {
                R::LOOM
            }

            fn factory(self, table: Arc<Mutex<HandleTable<T>>>) -> Factory {
                Box::new(move |_: &[Layout]| {
                    let mut f = self.clone();
                    let table = Arc::clone(&table);
                    Box::new(move |cx: &mut HostCx<'_>, r: &mut [u64]| {
                        let out = {
                            let mut t = table.lock().map_err(|_| POISONED.to_string())?;
                            let id = r[R::LEAD];
                            let item = t.items.get_mut(&id).ok_or_else(|| format!("stale handle {id}"))?;
                            let mut regs = r[R::LEAD + 1..].iter().copied();
                            f(item, $(arg!(regs, cx, ($($d)*))),+)
                        };
                        out.put(cx, r)
                    })
                })
            }
        }
    };
}

impl<F, R> IntoHost<()> for F
where
    F: FnMut() -> R + Clone + Send + 'static,
    R: Out,
{
    fn params() -> Vec<&'static str> {
        Vec::new()
    }

    fn ret() -> &'static str {
        R::LOOM
    }

    fn factory(self) -> Factory {
        Box::new(move |_: &[Layout]| {
            let mut f = self.clone();
            Box::new(move |cx: &mut HostCx<'_>, r: &mut [u64]| {
                let out = f();
                out.put(cx, r)
            })
        })
    }
}

impl<F, T, R> IntoMethod<T, ()> for F
where
    F: FnMut(&mut T) -> R + Clone + Send + 'static,
    T: Send + 'static,
    R: Out,
{
    fn params() -> Vec<&'static str> {
        Vec::new()
    }

    fn ret() -> &'static str {
        R::LOOM
    }

    fn factory(self, table: Arc<Mutex<HandleTable<T>>>) -> Factory {
        Box::new(move |_: &[Layout]| {
            let mut f = self.clone();
            let table = Arc::clone(&table);
            Box::new(move |cx: &mut HostCx<'_>, r: &mut [u64]| {
                let out = {
                    let mut t = table.lock().map_err(|_| POISONED.to_string())?;
                    let id = r[R::LEAD];
                    let item = t.items.get_mut(&id).ok_or_else(|| format!("stale handle {id}"))?;
                    f(item)
                };
                out.put(cx, r)
            })
        })
    }
}

// Functions: one impl per combination of argument kinds, arities 1 to 3.
free_impl!([A0] (N A0));
free_impl!([](T));
free_impl!([A0, A1] (N A0) (N A1));
free_impl!([A0] (N A0) (T));
free_impl!([A1] (T) (N A1));
free_impl!([](T)(T));
free_impl!([A0, A1, A2] (N A0) (N A1) (N A2));
free_impl!([A0, A1] (N A0) (N A1) (T));
free_impl!([A0, A2] (N A0) (T) (N A2));
free_impl!([A0] (N A0) (T) (T));
free_impl!([A1, A2] (T) (N A1) (N A2));
free_impl!([A1] (T) (N A1) (T));
free_impl!([A2] (T) (T) (N A2));
free_impl!([](T)(T)(T));

// Handle methods: one impl per combination of argument kinds, arities 1 to 2.
method_impl!([A0] (N A0));
method_impl!([](T));
method_impl!([A0, A1] (N A0) (N A1));
method_impl!([A0] (N A0) (T));
method_impl!([A1] (T) (N A1));
method_impl!([](T)(T));
