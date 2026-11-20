//! Back-ports of standard-library methods newer than Rust 1.78, the toolchain moco is pinned to (issue archive#100): the newest
//! one that every build machine, Lion included, can run.
//! Call sites stay identical to `main`; bring them into scope with `use crate::compat::*;`
//! (`use moco_core::compat::*;` in the front end). On a newer compiler the inherent methods win and these are unused.
#![allow(dead_code, clippy::wrong_self_convention)]

/// `Option::is_none_or` (stable in 1.82).
pub trait OptionIsNoneOr<T> {
    fn is_none_or(self, f: impl FnOnce(T) -> bool) -> bool;
}

impl<T> OptionIsNoneOr<T> for Option<T> {
    fn is_none_or(self, f: impl FnOnce(T) -> bool) -> bool {
        match self {
            None => true,
            Some(x) => f(x),
        }
    }
}

/// Unsigned `is_multiple_of` (stable in 1.87). Like std, `x.is_multiple_of(0)` is true only for `x == 0`.
pub trait IsMultipleOf: Sized {
    fn is_multiple_of(self, rhs: Self) -> bool;
}

macro_rules! is_multiple_of {
    ($($t:ty),*) => {$(
        impl IsMultipleOf for $t {
            fn is_multiple_of(self, rhs: Self) -> bool {
                if rhs == 0 { self == 0 } else { self % rhs == 0 }
            }
        }
    )*};
}
is_multiple_of!(u8, u16, u32, u64, u128, usize);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_and_integer_helpers() {
        assert!(None::<u8>.is_none_or(|_| false));
        assert!(Some(2).is_none_or(|x| x == 2));
        assert!(!Some(3).is_none_or(|x| x == 2));
        assert!(0usize.is_multiple_of(0) && !3usize.is_multiple_of(0) && 9usize.is_multiple_of(3) && !10u32.is_multiple_of(3));
    }
}
