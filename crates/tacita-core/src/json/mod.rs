//! Byte-exact JSON helpers.

mod scan;
mod string;

pub use scan::{Lit, ScanError, Seg, scan, scan_tolerant, splice};
pub use string::{Decoded, StrError, clean_lone_surrogates, decode, encode, replace};
