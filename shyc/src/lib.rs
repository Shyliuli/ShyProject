//! Building blocks for the independent, teaching-oriented ShyC compiler.
//!
//! This library is the whole pipeline: source locations, lexer, preprocessor,
//! parser, semantic types, typed CFG IR, optimization passes, both code
//! generators, and the C source/header export. The `shyc` binary is the
//! command-line driver over these modules.
pub mod abi;
pub mod abi_adapter;
pub mod analysis;
pub mod ast;
pub mod c;
pub mod config;
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
