//! Building blocks for the independent, teaching-oriented ShyC compiler.
//!
//! The frontend and backend are under development. This library currently
//! exposes source locations, lexical analysis, and the typed CFG IR; it is not
//! yet an alternative executable compiler.
pub mod analysis;
pub mod ast;
pub mod ir;
pub mod lexer;
pub mod literal;
pub mod parser;
pub mod passes;
pub mod preprocess;
pub mod sema;
pub mod source;
