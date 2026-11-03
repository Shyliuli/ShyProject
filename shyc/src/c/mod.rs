//! The C backend: one type printer shared by the source and header exports.
//!
//! The front end hands this module a translation unit after compile-time
//! expansion, so the only ShyC forms still present are the sugar the printer
//! resolves: inferred locals and trait/impl blocks. A trait value becomes a
//! structure of function pointers, an impl block becomes mangled functions plus
//! a constructor for that structure, and a method call becomes a call to the
//! mangled function.

pub mod source;
pub mod types;
