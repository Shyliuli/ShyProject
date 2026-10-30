//! Source-language types and target layout used by semantic analysis.
//!
//! Name binding and expression checking will build on these types; the syntax
//! AST itself deliberately contains no resolved symbols or implicit casts.
pub mod layout;
pub mod types;
