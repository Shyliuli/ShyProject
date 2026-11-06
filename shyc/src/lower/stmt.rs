//! Statement lowering and control-flow construction.
//!
//! Locals stay in memory slots, so joins never need phis: `&&`, `||`, `?:`,
//! and `switch` either use a temporary slot or a terminator. `goto` is patched
//! after the whole body is available, because a label may be defined after it
//! is used.

use super::{
    BreakContext, ExprKind, FnLower, LabelInfo, LoopContext, PendingGoto, Resolved, SwitchContext,
};
use crate::ast;
use crate::ir;
use crate::sema::types::{TypeId, TypeKind};
use crate::source::{Diagnostic, Span};

impl FnLower<'_> {
    pub(super) fn lower_block(&mut self, block: &ast::Block) -> Result<(), Diagnostic> {
        self.push_scope();
        for statement in &block.stmts {
            self.lower_stmt(statement)?;
        }
        self.pop_scope();
        Ok(())
    }

    /// Lower a block as a GNU statement expression, returning its tail value.
    pub(super) fn lower_block_value(
        &mut self,
        block: &ast::Block,
    ) -> Result<Option<(ir::Operand, TypeId)>, Diagnostic> {
        self.push_scope();
        let mut result = None;
        let count = block.stmts.len();
        for (index, statement) in block.stmts.iter().enumerate() {
            if index + 1 != count {
                self.lower_stmt(statement)?;
                continue;
            }
            match statement {
                ast::Stmt::Expr { expr, .. } => {
                    let value = self.lower_expr(expr)?;
                    if !matches!(value.kind, ExprKind::Void) {
                        result = Some(self.rvalue_val(value, expr.span())?);
                    }
                }
                ast::Stmt::Block(inner) => {
                    result = self.lower_block_value(inner)?;
                }
                _ => self.lower_stmt(statement)?,
            }
        }
        self.pop_scope();
        Ok(result)
    }

    fn push_scope(&mut self) {
        let id = self.scope_counter;
        self.scope_counter += 1;
        self.scopes.push(super::FnScope {
            id,
            defers: Vec::new(),
            bindings: Default::default(),
        });
        self.l.push_type_scope();
    }

    fn pop_scope(&mut self) {
        let index = self.scopes.len() - 1;
        self.emit_scope_defers(index);
        self.scopes.pop();
        self.l.pop_type_scope();
    }

    fn emit_scope_defers(&mut self, index: usize) {
        for local in self.scopes[index].defers.clone().into_iter().rev() {
            self.emit_defer_drop(local);
        }
    }

    /// Emit the guarded drops of every scope from `depth` outward, innermost
    /// scope and last declaration first.
    pub(super) fn emit_defers_from(&mut self, depth: usize) {
        for index in (depth..self.scopes.len()).rev() {
            self.emit_scope_defers(index);
        }
    }

    pub(super) fn finish_fallthrough(&mut self) {
        if self.terminated {
            return;
        }
        if self.l.types.is_void(self.return_ty) {
            self.terminate(ir::Terminator::ret(None));
        } else if self.is_main && self.l.types.is_integer(self.return_ty) {
            let zero = ir::Operand::Constant(ir::Constant::int(self.l.ir_type(self.return_ty), 0));
            self.terminate(ir::Terminator::ret(Some(zero)));
        } else {
            self.terminate(ir::Terminator::unreachable());
        }
    }

    fn lower_stmt(&mut self, statement: &ast::Stmt) -> Result<(), Diagnostic> {
        match statement {
            ast::Stmt::Empty(_) => Ok(()),
            ast::Stmt::Expr { expr, .. } => self.discard(expr),
            ast::Stmt::Decl(declaration) => self.lower_declaration(declaration),
            ast::Stmt::Let(declaration) => self.lower_let(declaration),
            ast::Stmt::Block(block) => self.lower_block(block),
            ast::Stmt::If {
                condition,
                then_branch,
                else_branch,
                span,
            } => self.lower_if(condition, then_branch, else_branch.as_deref(), *span),
            ast::Stmt::While {
                condition,
                body,
                span,
            } => self.lower_while(condition, body, *span),
            ast::Stmt::DoWhile {
                body,
                condition,
                span,
            } => self.lower_do_while(body, condition, *span),
            ast::Stmt::For {
                init,
                condition,
                step,
                body,
                span,
            } => self.lower_for(
                init.as_deref(),
                condition.as_deref(),
                step.as_deref(),
                body,
                *span,
            ),
            ast::Stmt::Switch {
                condition,
                body,
                span,
            } => self.lower_switch(condition, body, *span),
            ast::Stmt::Case {
                value,
                end,
                stmt,
                span,
            } => self.lower_case(value, end.as_ref(), stmt, *span),
            ast::Stmt::Default { stmt, span } => self.lower_default(stmt, *span),
            ast::Stmt::Label { name, stmt, span } => self.lower_label(name, stmt, *span),
            ast::Stmt::Goto { label, span } => self.lower_goto(label, *span),
            ast::Stmt::Break(span) => self.lower_break(*span),
            ast::Stmt::Continue(span) => self.lower_continue(*span),
            ast::Stmt::Return { value, span } => self.lower_return(value.as_ref(), *span),
            ast::Stmt::Asm(asm) => self.lower_asm(asm),
            ast::Stmt::AllowDeferCopy { stmt, .. } => {
                let saved = self.allow_defer_copy;
                self.allow_defer_copy = true;
                let result = self.lower_stmt(stmt);
                self.allow_defer_copy = saved;
                result
            }
        }
    }

    /// Lower one branch of an `if`, loop, or `switch`.
    ///
    /// A `defer` declaration written there without braces owns only that
    /// statement, so it is lowered in a scope of its own and dropped as soon as
    /// the branch statement ends.
    fn lower_branch_statement(&mut self, statement: &ast::Stmt) -> Result<(), Diagnostic> {
        if !statement.is_defer_declaration() {
            return self.lower_stmt(statement);
        }
        self.push_scope();
        let result = self.lower_stmt(statement);
        self.pop_scope();
        result
    }

    fn lower_if(
        &mut self,
        condition: &ast::Expr,
        then_branch: &ast::Stmt,
        else_branch: Option<&ast::Stmt>,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let (operand, ty) = self.rvalue(condition)?;
        let condition = self.boolean_operand(operand, ty, span)?;
        let then_block = self.f.alloc_block();
        let else_block = self.f.alloc_block();
        let end_block = self.f.alloc_block();
        self.terminate(ir::Terminator::branch(condition, then_block, else_block));
        self.set_current(then_block);
        self.lower_branch_statement(then_branch)?;
        if !self.terminated {
            self.jump_to(end_block);
        }
        self.set_current(else_block);
        if let Some(else_branch) = else_branch {
            self.lower_branch_statement(else_branch)?;
        }
        if !self.terminated {
            self.jump_to(end_block);
        }
        self.set_current(end_block);
        Ok(())
    }

    fn lower_while(
        &mut self,
        condition: &ast::Expr,
        body: &ast::Stmt,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let condition_block = self.f.alloc_block();
        let body_block = self.f.alloc_block();
        let end_block = self.f.alloc_block();
        self.jump_to(condition_block);
        self.set_current(condition_block);
        let (operand, ty) = self.rvalue(condition)?;
        let condition = self.boolean_operand(operand, ty, span)?;
        self.terminate(ir::Terminator::branch(condition, body_block, end_block));
        let depth = self.scopes.len();
        self.set_current(body_block);
        self.loops.push(LoopContext {
            continue_target: condition_block,
            continue_depth: depth,
        });
        self.breaks.push(BreakContext {
            target: end_block,
            depth,
        });
        self.lower_branch_statement(body)?;
        self.loops.pop();
        self.breaks.pop();
        if !self.terminated {
            self.jump_to(condition_block);
        }
        self.set_current(end_block);
        Ok(())
    }

    fn lower_do_while(
        &mut self,
        body: &ast::Stmt,
        condition: &ast::Expr,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let body_block = self.f.alloc_block();
        let condition_block = self.f.alloc_block();
        let end_block = self.f.alloc_block();
        self.jump_to(body_block);
        let depth = self.scopes.len();
        self.set_current(body_block);
        self.loops.push(LoopContext {
            continue_target: condition_block,
            continue_depth: depth,
        });
        self.breaks.push(BreakContext {
            target: end_block,
            depth,
        });
        self.lower_branch_statement(body)?;
        self.loops.pop();
        self.breaks.pop();
        if !self.terminated {
            self.jump_to(condition_block);
        }
        self.set_current(condition_block);
        let (operand, ty) = self.rvalue(condition)?;
        let condition = self.boolean_operand(operand, ty, span)?;
        self.terminate(ir::Terminator::branch(condition, body_block, end_block));
        self.set_current(end_block);
        Ok(())
    }

    fn lower_for(
        &mut self,
        init: Option<&ast::ForInit>,
        condition: Option<&ast::Expr>,
        step: Option<&ast::Expr>,
        body: &ast::Stmt,
        span: Span,
    ) -> Result<(), Diagnostic> {
        self.push_scope();
        if let Some(init) = init {
            match init {
                ast::ForInit::Decl(declaration) => self.lower_declaration(declaration)?,
                ast::ForInit::Expr(expr) => self.discard(expr)?,
                ast::ForInit::Let(decl) => self.lower_let(decl)?,
            }
        }
        let condition_block = self.f.alloc_block();
        let body_block = self.f.alloc_block();
        let step_block = self.f.alloc_block();
        let end_block = self.f.alloc_block();
        self.jump_to(condition_block);
        self.set_current(condition_block);
        let condition = match condition {
            Some(condition) => {
                let (operand, ty) = self.rvalue(condition)?;
                self.boolean_operand(operand, ty, span)?
            }
            None => ir::Operand::Constant(ir::Constant::int(ir::Type::I1, 1)),
        };
        self.terminate(ir::Terminator::branch(condition, body_block, end_block));
        let break_depth = self.scopes.len() - 1;
        let continue_depth = self.scopes.len();
        self.set_current(body_block);
        self.breaks.push(BreakContext {
            target: end_block,
            depth: break_depth,
        });
        self.loops.push(LoopContext {
            continue_target: step_block,
            continue_depth,
        });
        self.lower_branch_statement(body)?;
        self.loops.pop();
        self.breaks.pop();
        if !self.terminated {
            self.jump_to(step_block);
        }
        self.set_current(step_block);
        if let Some(step) = step {
            self.discard(step)?;
        }
        self.jump_to(condition_block);
        self.set_current(end_block);
        self.pop_scope();
        Ok(())
    }

    fn lower_switch(
        &mut self,
        condition: &ast::Expr,
        body: &ast::Stmt,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let value = self.lower_expr(condition)?;
        let scrutinee = value.ty;
        // C-compatible niche values store the payload itself. Normalize it to
        // the logical variant tag (zero/nonzero) before the ordinary switch.
        let (operand, ty) = if self.l.types.is_niche_enum(scrutinee) {
            let qualifiers = value.qualifiers;
            let address = self.object_address(value, span)?;
            let underlying = self
                .l
                .types
                .enum_underlying(scrutinee)
                .expect("niche enum underlying type");
            let raw = self.load_value(address, underlying, qualifiers);
            let raw_ty = self.l.ir_type(underlying);
            let nonzero = self.emit_value(ir::InstKind::ICmp {
                pred: ir::IntPredicate::Ne,
                lhs: raw,
                rhs: ir::Operand::Constant(ir::Constant::zero(raw_ty)),
                ty: raw_ty,
            });
            (
                self.bool_to_int(ir::Operand::Value(nonzero)),
                self.int_type(),
            )
        } else if self.l.types.is_payload_enum(scrutinee) {
            let qualifiers = value.qualifiers;
            let address = self.object_address(value, span)?;
            let Some(tag_ty) = self.l.types.enum_underlying(scrutinee) else {
                return Err(Diagnostic::new(span, "payload enum has no underlying type"));
            };
            (self.load_value(address, tag_ty, qualifiers), tag_ty)
        } else {
            self.rvalue_val(value, span)?
        };
        if !self.l.types.is_integer(ty) {
            return Err(Diagnostic::new(
                span,
                "switch condition needs an integer or enum type",
            ));
        }
        let promoted = self.l.integer_promotion(ty);
        let operand = self.convert(operand, ty, promoted, span, false)?;
        let ir_ty = self.l.ir_type(promoted);
        let end_block = self.f.alloc_block();
        let body_block = self.f.alloc_block();
        let terminator_block = self.block;
        let break_depth = self.scopes.len();
        self.set_current(body_block);
        self.breaks.push(BreakContext {
            target: end_block,
            depth: break_depth,
        });
        self.switches.push(SwitchContext {
            value: operand,
            value_ty: promoted,
            ir_ty,
            scrutinee,
            terminator_block,
            default: None,
            cases: Vec::new(),
            ranges: Vec::new(),
            labels: Vec::new(),
            end: end_block,
            span,
        });
        self.lower_branch_statement(body)?;
        self.breaks.pop();
        let context = self.switches.pop().expect("switch context");
        self.check_switch_exhaustiveness(&context)?;
        if !self.terminated {
            self.jump_to(end_block);
        }
        let default = context.default.unwrap_or(context.end);
        // A range label cannot live in a switch table, so each one is a pair of
        // comparisons in front of the dispatch.
        self.set_current(context.terminator_block);
        for (low, high, block) in &context.ranges {
            let in_high = self.f.alloc_block();
            let next = self.f.alloc_block();
            let above = self.emit_value(ir::InstKind::ICmp {
                pred: ir::IntPredicate::Sge,
                lhs: context.value,
                rhs: ir::Operand::Constant(*low),
                ty: context.ir_ty,
            });
            self.terminate(ir::Terminator::branch(
                ir::Operand::Value(above),
                in_high,
                next,
            ));
            self.set_current(in_high);
            let below = self.emit_value(ir::InstKind::ICmp {
                pred: ir::IntPredicate::Sle,
                lhs: context.value,
                rhs: ir::Operand::Constant(*high),
                ty: context.ir_ty,
            });
            self.terminate(ir::Terminator::branch(
                ir::Operand::Value(below),
                *block,
                next,
            ));
            self.set_current(next);
        }
        self.terminate(ir::Terminator::Switch {
            value: context.value,
            ty: context.ir_ty,
            default,
            cases: context.cases,
            span: Some(context.span),
        });
        self.set_current(end_block);
        Ok(())
    }

    /// Closed payload enums need a case for every variant; ordinary C enums
    /// are open and need a `default` unless the file attribute disables the
    /// check. A `default` or `_` label suppresses either check.
    fn check_switch_exhaustiveness(&self, context: &SwitchContext) -> Result<(), Diagnostic> {
        if context.default.is_some() {
            return Ok(());
        }
        if self.l.types.is_payload_enum(context.scrutinee) {
            let Some(info) = self.l.types.enum_info(context.scrutinee) else {
                return Ok(());
            };
            let missing: Vec<&str> = info
                .variants
                .iter()
                .filter(|variant| {
                    let tag = ir::Constant::int(context.ir_ty, variant.value);
                    !context.cases.iter().any(|case| case.value == tag)
                })
                .map(|variant| variant.name.as_str())
                .collect();
            if !missing.is_empty() {
                let names = missing
                    .iter()
                    .map(|name| format!("'{name}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let missing = if missing.len() == 1 {
                    format!("is missing a case for variant {names}")
                } else {
                    format!("is missing cases for variants {names}")
                };
                return Err(Diagnostic::new(
                    context.span,
                    format!(
                        "switch on a payload enum {missing}; add the missing `case` or a `default`/`_` label"
                    ),
                ));
            }
            return Ok(());
        }
        if matches!(self.l.types.kind(context.scrutinee), TypeKind::Enum(_))
            && !self.l.disable_nonexhaustive_switch
        {
            return Err(Diagnostic::new(
                context.span,
                "switch on a plain C enum needs a `default` or `_` label; add one, or put `#![disable_nonexhaustive_switch]` in the main input file",
            ));
        }
        Ok(())
    }

    fn lower_case(
        &mut self,
        value: &ast::Expr,
        end: Option<&ast::Expr>,
        statement: &ast::Stmt,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let Some(context) = self.switches.last() else {
            return Err(Diagnostic::new(span, "`case` outside a switch"));
        };
        if end.is_some() && self.l.types.is_niche_enum(context.scrutinee) {
            return Err(Diagnostic::new(
                span,
                "case ranges need an integer switch condition",
            ));
        }
        let low = self.case_constant(value, span)?;
        let low_number = self.case_number(value)?;
        let (high, high_number) = match end {
            Some(end) => (self.case_constant(end, end.span())?, self.case_number(end)?),
            None => (low, low_number),
        };
        if low_number > high_number {
            return Err(Diagnostic::new(span, "case range is written backwards"));
        }
        let context = self.switches.last_mut().expect("switch context");
        let overlapping = context
            .labels
            .iter()
            .find(|(low, high)| low_number <= *high && *low <= high_number)
            .copied();
        if let Some((low, high)) = overlapping {
            let existing_is_range = low != high;
            let message = match (end.is_some(), existing_is_range) {
                (false, false) => "duplicate case value",
                (true, true) => "case range overlaps another case range",
                (true, false) => "case range covers an existing case value",
                (false, true) => "case value is already covered by a case range",
            };
            return Err(Diagnostic::new(span, message));
        }
        context.labels.push((low_number, high_number));
        let block = self.f.alloc_block();
        if !self.terminated {
            self.jump_to(block);
        }
        self.set_current(block);
        let context = self.switches.last_mut().expect("switch context");
        if end.is_some() {
            context.ranges.push((low, high, block));
        } else {
            context.cases.push(ir::SwitchCase::new(low, block));
        }
        self.lower_stmt(statement)
    }

    /// The tag a bare variant name denotes as a `case` label.
    ///
    /// A switch over a payload enum lets `case Some:` stand for
    /// `case Option<Str>::Some:`. C keeps its own meaning: a bare name that is
    /// already a constant keeps it, so the shorthand never changes a C label.
    fn case_variant_tag(&self, expr: &ast::Expr) -> Option<i64> {
        let ast::Expr::Name(name) = expr else {
            return None;
        };
        let context = self.switches.last()?;
        if self.l.global_ref(&name.text).is_some() || self.l.enum_consts.contains_key(&name.text) {
            return None;
        }
        self.l
            .types
            .enum_variant(context.scrutinee, &name.text)
            .map(|variant| variant.value)
    }

    /// The numeric label of a `case` value, for duplicate detection.
    fn case_number(&mut self, expr: &ast::Expr) -> Result<i64, Diagnostic> {
        match self.case_variant_tag(expr) {
            Some(tag) => Ok(tag),
            None => self.l.eval_const_int(expr),
        }
    }

    /// The constant a case label names, converted to the switch value's type.
    fn case_constant(&mut self, expr: &ast::Expr, span: Span) -> Result<ir::Constant, Diagnostic> {
        if let Some(tag) = self.case_variant_tag(expr) {
            let ir_ty = self.switches.last().expect("switch context").ir_ty;
            return Ok(ir::Constant::int(ir_ty, tag));
        }
        let context = self.switches.last().expect("switch context");
        if self.l.types.is_niche_enum(context.scrutinee) {
            let tag = self.l.eval_const_int(expr)?;
            Ok(ir::Constant::int(context.ir_ty, tag))
        } else {
            let value_ty = context.value_ty;
            let value = self.l.eval_const(expr)?;
            let value = self.l.const_cast(value, value_ty, span)?;
            self.l.const_int_of_ir(&value, span)
        }
    }

    fn lower_default(&mut self, statement: &ast::Stmt, span: Span) -> Result<(), Diagnostic> {
        if self.switches.last().is_none() {
            return Err(Diagnostic::new(span, "`default` or `_` outside a switch"));
        }
        let block = self.f.alloc_block();
        if !self.terminated {
            self.jump_to(block);
        }
        self.set_current(block);
        let context = self.switches.last_mut().expect("switch context");
        if context.default.is_some() {
            return Err(Diagnostic::new(span, "duplicate `default`/`_` label"));
        }
        context.default = Some(block);
        self.lower_stmt(statement)
    }

    fn scope_path(&self) -> Vec<(u32, Vec<usize>)> {
        self.scopes
            .iter()
            .map(|scope| (scope.id, scope.defers.clone()))
            .collect()
    }

    fn lower_label(
        &mut self,
        name: &ast::Name,
        statement: &ast::Stmt,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self.labels.contains_key(&name.text) {
            return Err(Diagnostic::new(
                span,
                format!("duplicate label '{}'", name.text),
            ));
        }
        let block = self.f.alloc_block();
        if !self.terminated {
            self.jump_to(block);
        }
        self.set_current(block);
        let path = self.scope_path();
        self.labels
            .insert(name.text.clone(), LabelInfo { block, path });
        self.lower_stmt(statement)
    }

    fn lower_goto(&mut self, label: &ast::Name, span: Span) -> Result<(), Diagnostic> {
        self.ensure_block();
        let source = self.block;
        let trampoline = self.f.alloc_block();
        let path = self.scope_path();
        self.pending_gotos.push(PendingGoto {
            label: label.text.clone(),
            source,
            trampoline,
            span,
            path,
        });
        self.terminate(ir::Terminator::jump(trampoline));
        Ok(())
    }

    pub(super) fn patch_gotos(&mut self) -> Result<(), Diagnostic> {
        let gotos = std::mem::take(&mut self.pending_gotos);
        for pending in gotos {
            let Some(label) = self.labels.get(&pending.label).cloned() else {
                return Err(Diagnostic::new(
                    pending.span,
                    format!("use of undeclared label '{}'", pending.label),
                ));
            };
            let mut common = 0;
            while common < pending.path.len()
                && common < label.path.len()
                && pending.path[common].0 == label.path[common].0
            {
                common += 1;
            }
            if label.path[common..]
                .iter()
                .any(|(_, locals)| !locals.is_empty())
            {
                return Err(Diagnostic::new(
                    pending.span,
                    "goto jumps into the scope of a defer declaration",
                ));
            }
            // The scopes the jump leaves, innermost first and each in reverse
            // declaration order.
            let exited: Vec<usize> = pending.path[common..]
                .iter()
                .rev()
                .flat_map(|(_, locals)| locals.iter().rev().copied())
                .collect();
            if !exited.is_empty() {
                let cleanup = self.f.alloc_block();
                self.f.block_mut(pending.source).terminator = ir::Terminator::jump(cleanup);
                let saved_block = self.block;
                let saved_terminated = self.terminated;
                self.set_current(cleanup);
                for local in exited {
                    self.emit_defer_drop(local);
                }
                self.jump_to(label.block);
                self.block = saved_block;
                self.terminated = saved_terminated;
            } else {
                self.f.block_mut(pending.source).terminator = ir::Terminator::jump(label.block);
            }
            self.f.remove_block(pending.trampoline);
        }
        Ok(())
    }

    fn lower_break(&mut self, span: Span) -> Result<(), Diagnostic> {
        let Some(context) = self.breaks.last() else {
            return Err(Diagnostic::new(span, "`break` outside a loop or switch"));
        };
        let target = context.target;
        let depth = context.depth;
        self.emit_defers_from(depth);
        self.jump_to(target);
        Ok(())
    }

    fn lower_continue(&mut self, span: Span) -> Result<(), Diagnostic> {
        let Some(context) = self.loops.last() else {
            return Err(Diagnostic::new(span, "`continue` outside a loop"));
        };
        let target = context.continue_target;
        let depth = context.continue_depth;
        self.emit_defers_from(depth);
        self.jump_to(target);
        Ok(())
    }

    fn lower_return(&mut self, value: Option<&ast::Expr>, span: Span) -> Result<(), Diagnostic> {
        if self.l.types.is_void(self.return_ty) {
            if let Some(value) = value {
                let expression = self.lower_expr(value)?;
                if !matches!(expression.kind, ExprKind::Void) {
                    return Err(Diagnostic::new(span, "void function cannot return a value"));
                }
            }
            self.emit_defers_from(0);
            self.terminate(ir::Terminator::ret(None));
            return Ok(());
        }
        let Some(value) = value else {
            return Err(Diagnostic::new(
                span,
                "non-void function must return a value",
            ));
        };
        let (operand, ty) = self.return_value(value)?;
        let operand = self.convert(operand, ty, self.return_ty, span, false)?;
        self.emit_defers_from(0);
        self.terminate(ir::Terminator::ret(Some(operand)));
        Ok(())
    }

    /// Lower a returned expression. `return s` on a defer local is
    /// `return s.move()`: the value is read and the moved flag set, so the
    /// exit drops skip this one local.
    fn return_value(&mut self, value: &ast::Expr) -> Result<(ir::Operand, TypeId), Diagnostic> {
        if let ast::Expr::Name(name) = value
            && let Ok(Resolved::Local(local)) = self.resolve_name(&name.text, name.span)
            && self.locals[local].moved_slot.is_some()
        {
            let moved = self.move_defer_local(local);
            return self.rvalue_val(moved, name.span);
        }
        self.rvalue(value)
    }
}
