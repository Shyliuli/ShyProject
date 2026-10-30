//! Building blocks for the independent, teaching-oriented ShyC compiler.
//!
//! The frontend and backend are under development. This library currently
//! exposes source locations, lexical analysis, and the typed CFG IR; it is not
//! yet an alternative executable compiler.
pub mod ir;
pub mod lexer;
pub mod source;
