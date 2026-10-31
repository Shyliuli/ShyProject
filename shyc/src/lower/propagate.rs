//! Postfix `?` propagation over transparent `Option` and `Result` payload
//! enums.
//!
//! The shape is structural: an enum with `None` and `Some(payload)` is an
//! Option, and one with `Err(error)` and `Ok(payload)` is a Result. There is no
//! hidden runtime, trait, or allocation: the tag is loaded from offset 0, the
//! success payload is copied out of the union, and a failure builds the
//! enclosing function's failure variant and returns it. The caller's ordinary
//! RAII drops run on that early return.

use super::type_of::TryShape;
use super::{ExprKind, ExprVal, FnLower};
use crate::ast;
use crate::ir;
use crate::sema::types::{Qualifiers, TypeId};
use crate::source::{Diagnostic, Span};

impl FnLower<'_> {
    /// Lower `value?`: yield the success payload, or return the failure from
    /// the enclosing function.
    pub(super) fn lower_try(
        &mut self,
        value: &ast::Expr,
        span: Span,
    ) -> Result<ExprVal, Diagnostic> {
        let value = self.lower_expr(value)?;
        let source_ty = value.ty;
        let qualifiers = value.qualifiers;
        let source = self.l.try_shape(source_ty).ok_or_else(|| {
            Diagnostic::new(
                span,
                "`?` needs an Option (`None`/`Some`) or Result (`Err`/`Ok`) payload enum",
            )
        })?;
        let target_ty = self.return_ty;
        let target = self.l.try_shape(target_ty).ok_or_else(|| {
            Diagnostic::new(
                span,
                format!(
                    "`?` propagates {} but the enclosing function does not return an Option or Result",
                    source.family.article()
                ),
            )
        })?;
        if source.family != target.family {
            return Err(Diagnostic::new(
                span,
                format!(
                    "`?` propagates {} but the enclosing function returns {}",
                    source.family.article(),
                    target.family.article()
                ),
            ));
        }
        let success_ty = source.success.ok_or_else(|| {
            Diagnostic::new(span, "`?` needs a success payload; this Result has none")
        })?;
        check_error_payload(&self.l.types, source, target, span)?;

        let address = self.object_address(value, span)?;
        let Some(underlying) = self.l.types.enum_underlying(source_ty) else {
            return Err(Diagnostic::new(span, "payload enum has no underlying type"));
        };
        let tag_ty = self.l.ir_type(underlying);
        let tag = self.load_value(address.clone(), underlying, qualifiers);
        let offset = payload_offset(&self.l.types, source_ty);
        let displacement = address.offset + i64::from(offset);
        let payload_address = address.with_offset(displacement);
        let failure_tag = ir::Operand::Constant(ir::Constant::int(tag_ty, source.failure_tag));
        let is_failure = self.emit_value(ir::InstKind::ICmp {
            pred: ir::IntPredicate::Eq,
            lhs: tag,
            rhs: failure_tag,
            ty: tag_ty,
        });

        let failure_block = self.f.alloc_block();
        let success_block = self.f.alloc_block();
        self.terminate(ir::Terminator::branch(
            ir::Operand::Value(is_failure),
            failure_block,
            success_block,
        ));

        // Failure: rebuild the enclosing function's failure variant and return
        // it. This is an early return, so the regular drop path runs first.
        self.set_current(failure_block);
        let error_payload = match (source.failure, target.failure) {
            (Some(source_error), Some(target_error)) => {
                let operand = self.load_value(payload_address.clone(), source_error, qualifiers);
                Some((operand, target_error))
            }
            _ => None,
        };
        let failure =
            self.build_payload_enum(target_ty, target.failure_tag, error_payload, span)?;
        let (failure_operand, _) = self.rvalue_val(failure, span)?;
        self.emit_drops_on_exit(0);
        self.terminate(ir::Terminator::ret(Some(failure_operand)));

        // Success: copy the payload into a temporary and hand it back as a
        // place, so aggregate payloads keep working without a hidden move.
        self.set_current(success_block);
        let operand = self.load_value(payload_address, success_ty, qualifiers);
        let slot = self.temp_slot(success_ty, "");
        self.store_value(
            ir::Address::slot(slot),
            operand,
            success_ty,
            Qualifiers::NONE,
        );
        Ok(ExprVal {
            ty: success_ty,
            qualifiers: Qualifiers::NONE,
            kind: ExprKind::Place(ir::Address::slot(slot)),
        })
    }
}

/// Require a propagated Result error payload to fit the destination exactly.
///
/// Success payloads may differ, because `?` yields the source value and the
/// enclosing expression converts it. The error payload is copied wholesale, so
/// a missing or incompatible one is diagnosed instead of silently dropped.
fn check_error_payload(
    types: &crate::sema::types::TypeStore,
    source: TryShape,
    target: TryShape,
    span: Span,
) -> Result<(), Diagnostic> {
    match (source.failure, target.failure) {
        (None, None) => Ok(()),
        (Some(source), Some(target)) if types.is_compatible(source, target) => Ok(()),
        _ => Err(Diagnostic::new(
            span,
            "`?` cannot propagate this error payload to the enclosing return type",
        )),
    }
}

fn payload_offset(types: &crate::sema::types::TypeStore, ty: TypeId) -> u32 {
    types
        .enum_info(ty)
        .map(|info| info.payload_offset)
        .unwrap_or(0)
}
