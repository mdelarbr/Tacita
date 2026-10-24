//! Byte-exact JSON helpers.

mod string;

pub use string::{Decoded, StrError, decode, encode, replace};
