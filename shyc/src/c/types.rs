//! The single place a C type is spelled.
//!
//! Both the header and the source backend print through these functions, so a
//! declaration cannot drift between the two outputs. They still live in the
//! export module and move here once the header export is rewritten.

pub(crate) use crate::export::{declaration_text, declarator_text, specs_text};
