//! Statement lowering and control-flow construction.
//!
//! Locals stay in memory slots, so joins never need phis: `&&`, `||`, `?:`,
//! and `switch` either use a temporary slot or a terminator. `goto` is patched
//! after the whole body is available, because a label may be defined after it
//! is used.

use super::{
    BreakContext, ExprKind, FnLower, LabelInfo, LoopContext, PendingGoto, SwitchContext,
};
use crate::ast;
use crate::ir;
use crate::sema::types::TypeId;
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
            drops: Vec::new(),
            bindings: Default::default(),
        });
        self.l.push_type_scope();
    }

    fn pop_scope(&mut self) {
        let index = self.scopes.len() - 1;
        self.emit_scope_drops(index);
        self.scopes.pop();
        self.l.pop_type_scope();
    }

    fn emit_scope_drops(&mut self, index: usize) {
        let drops = self.scopes[index].drops.clone();
        for local in drops.into_iter().rev() {
            self.emit_drop(local);
        }
    }

    pub(super) fn emit_drops_from(&mut self, depth: usize) {
        for index in (depth..self.scopes.len()).rev() {
            self.emit_scope_drops(index);
        }
    }

    pub(super) fn emit_all_drops(&mut self) {
        self.emit_drops_from(0);
    }

    pub(super) fn finish_fallthrough(&mut self) {
        if self.terminated {
            return;
        }
        if self.l.types.is_void(self.return_ty) {
            self.terminate(ir::Terminator::ret(None));
        } else if self.is_main && self.l.types.is_integer(self.return_ty) {
            let zero = ir::Operand::Constant(ir::Constant::int(
                self.l.ir_type(self.return_ty),
                0,
            ));
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
            ast::Stmt::Case { value, stmt, span } => self.lower_case(value, stmt, *span),
            ast::Stmt::Default { stmt, span } => self.lower_default(stmt, *span),
            ast::Stmt::Label { name, stmt, span } => self.lower_label(name, stmt, *span),
            ast::Stmt::Goto { label, span } => self.lower_goto(label, *span),
            ast::Stmt::Break(span) => self.lower_break(*span),
            ast::Stmt::Continue(span) => self.lower_continue(*span),
            ast::Stmt::Return { value, span } => self.lower_return(value.as_ref(), *span),
            ast::Stmt::Asm(asm) => self.lower_asm(asm),
        }
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
        self.terminate(ir::Terminator::branch(
            condition,
            then_block,
            else_block,
        ));
        self.set_current(then_block);
        self.lower_stmt(then_branch)?;
        if !self.terminated {
            self.jump_to(end_block);
        }
        self.set_current(else_block);
        if let Some(else_branch) = else_branch {
            self.lower_stmt(else_branch)?;
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
        self.terminate(ir::Terminator::branch(
            condition,
            body_block,
            end_block,
        ));
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
        self.lower_stmt(body)?;
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
        self.lower_stmt(body)?;
        self.loops.pop();
        self.breaks.pop();
        if !self.terminated {
            self.jump_to(condition_block);
        }
        self.set_current(condition_block);
        let (operand, ty) = self.rvalue(condition)?;
        let condition = self.boolean_operand(operand, ty, span)?;
        self.terminate(ir::Terminator::branch(
            condition,
            body_block,
            end_block,
        ));
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
        self.terminate(ir::Terminator::branch(
            condition,
            body_block,
            end_block,
        ));
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
        self.lower_stmt(body)?;
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
        let (operand, ty) = self.rvalue(condition)?;
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
            terminator_block,
            default: None,
            cases: Vec::new(),
            end: end_block,
            span,
        });
        self.lower_stmt(body)?;
        self.breaks.pop();
        let context = self.switches.pop().expect("switch context");
        if !self.terminated {
            self.jump_to(end_block);
        }
        let default = context.default.unwrap_or(context.end);
        let terminator = ir::Terminator::Switch {
            value: context.value,
            ty: context.ir_ty,
            default,
            cases: context.cases,
            span: Some(context.span),
        };
        self.f.block_mut(context.terminator_block).terminator = terminator;
        self.set_current(end_block);
        Ok(())
    }

    fn lower_case(
        &mut self,
        value: &ast::Expr,
        statement: &ast::Stmt,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let Some(context) = self.switches.last() else {
            return Err(Diagnostic::new(span, "`case` outside a switch"));
        };
        let value_ty = context.value_ty;
        let value = self.l.eval_const(value)?;
        let value = self.l.const_cast(value, value_ty, span)?;
        let constant = self.l.const_int_of_ir(&value, span)?;
        let block = self.f.alloc_block();
        if !self.terminated {
            self.jump_to(block);
        }
        self.set_current(block);
        let context = self.switches.last_mut().expect("switch context");
        if context
            .cases
            .iter()
            .any(|case| case.value == constant)
        {
            return Err(Diagnostic::new(span, "duplicate case value"));
        }
        context.cases.push(ir::SwitchCase::new(constant, block));
        self.lower_stmt(statement)
    }

    fn lower_default(
        &mut self,
        statement: &ast::Stmt,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self.switches.last().is_none() {
            return Err(Diagnostic::new(span, "`default` outside a switch"));
        }
        let block = self.f.alloc_block();
        if !self.terminated {
            self.jump_to(block);
        }
        self.set_current(block);
        let context = self.switches.last_mut().expect("switch context");
        if context.default.is_some() {
            return Err(Diagnostic::new(span, "duplicate `default` label"));
        }
        context.default = Some(block);
        self.lower_stmt(statement)
    }

    fn scope_path(&self) -> Vec<(u32, usize)> {
        self.scopes
            .iter()
            .map(|scope| {
                let active = scope
                    .drops
                    .iter()
                    .filter(|&&local| self.locals[local].drop_active)
                    .count();
                (scope.id, active)
            })
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
        self.labels.insert(
            name.text.clone(),
            LabelInfo { block, path },
        );
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
            if pending.path[common..].iter().any(|(_, count)| *count > 0) {
                return Err(Diagnostic::new(
                    pending.span,
                    "goto jumps out of the scope of an object with a drop method",
                ));
            }
            if label.path[common..].iter().any(|(_, count)| *count > 0) {
                return Err(Diagnostic::new(
                    pending.span,
                    "goto jumps into the scope of an object with a drop method",
                ));
            }
            self.f.block_mut(pending.source).terminator = ir::Terminator::jump(label.block);
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
        self.emit_drops_from(depth);
        self.jump_to(target);
        Ok(())
    }

    fn lower_continue(&mut self, span: Span) -> Result<(), Diagnostic> {
        let Some(context) = self.loops.last() else {
            return Err(Diagnostic::new(span, "`continue` outside a loop"));
        };
        let target = context.continue_target;
        let depth = context.continue_depth;
        self.emit_drops_from(depth);
        self.jump_to(target);
        Ok(())
    }

    fn lower_return(
        &mut self,
        value: Option<&ast::Expr>,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self.l.types.is_void(self.return_ty) {
            if let Some(value) = value {
                let expression = self.lower_expr(value)?;
                if !matches!(expression.kind, ExprKind::Void) {
                    return Err(Diagnostic::new(
                        span,
                        "void function cannot return a value",
                    ));
                }
            }
            self.emit_all_drops();
            self.terminate(ir::Terminator::ret(None));
            return Ok(());
        }
        let Some(value) = value else {
            return Err(Diagnostic::new(
                span,
                "non-void function must return a value",
            ));
        };
        let (operand, ty) = self.rvalue(value)?;
        let operand = self.convert(operand, ty, self.return_ty, span, false)?;
        self.deactivate_if_local(value);
        self.emit_all_drops();
        self.terminate(ir::Terminator::ret(Some(operand)));
        Ok(())
    }
}
