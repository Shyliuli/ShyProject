//! Building blocks for the independent, teaching-oriented ShyC compiler.
//!
//! The frontend and backend are under development. This library currently
//! exposes source locations, lexical analysis, and the typed CFG IR; it is not
//! yet an alternative executable compiler.
pub mod abi;
pub mod abi_adapter;
pub mod analysis;
pub mod ast;
pub mod export;
pub mod export_instances;
pub mod ir;
pub mod lexer;
pub mod literal;
pub mod llvm;
pub mod lower;
pub mod parser;
pub mod passes;
pub mod preprocess;
pub mod sema;
pub mod shy;
pub mod source;
