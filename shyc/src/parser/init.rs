//! Initializers, including nested braces and C99 designators.

use crate::ast::{Designator, Initializer, InitializerEntry, InitializerKind};
use crate::source::Diagnostic;

use super::Parser;

impl Parser {
    /// An initializer: `= expr`, or a brace-enclosed list.
    pub(crate) fn initializer(&mut self) -> Result<Initializer, Diagnostic> {
        if self.at_punct("{") {
            return self.initializer_list();
        }
        let start = self.here();
        let expr = self.assignment()?;
        Ok(Initializer {
            span: self.finish(start),
            kind: InitializerKind::Expr(Box::new(expr)),
        })
    }

    fn initializer_list(&mut self) -> Result<Initializer, Diagnostic> {
        let open = self.expect_punct("{")?;
        let mut entries = Vec::new();
        while !self.at_punct("}") {
            if self.at_eof() {
                return Err(Diagnostic::new(
                    self.here(),
                    "unterminated initializer list",
                ));
            }
            let start = self.here();
            let mut designators = Vec::new();
            loop {
                if self.eat_punct(".") {
                    let field = self.expect_ident()?;
                    designators.push(Designator::Field(field));
                } else if self.at_punct("[") {
                    self.bump();
                    let index = self.conditional()?;
                    if self.at_punct("...") {
                        return Err(Diagnostic::new(
                            self.here(),
                            "array designator ranges are not supported",
                        ));
                    }
                    self.expect_punct("]")?;
                    designators.push(Designator::Index(index));
                } else {
                    break;
                }
            }
            if !designators.is_empty() && !self.eat_punct("=") {
                return Err(Diagnostic::new(
                    self.here(),
                    "expected '=' after a designator",
                ));
            }
            let value = self.initializer()?;
            let span = self.finish(start);
            entries.push(InitializerEntry {
                span,
                designators,
                value,
            });
            if !self.eat_punct(",") {
                break;
            }
        }
        let close = self.expect_punct("}")?;
        Ok(Initializer {
            span: open.join(close),
            kind: InitializerKind::List(entries),
        })
    }
}
