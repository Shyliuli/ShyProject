use super::{AutoMethod, DerefStep, Lowerer, MethodInfo, RecordPath};
use crate::ast;
use crate::sema::types::{TypeId, TypeKind};
use crate::source::Diagnostic;
use std::collections::HashSet;

pub(super) const MAX_AUTO_DEREF: usize = 8;

impl Lowerer {
    pub(super) fn record_paths(
        &mut self,
        ty: TypeId,
        mutable: bool,
        span: crate::source::Span,
    ) -> Result<Vec<RecordPath>, Diagnostic> {
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        let mut current = ty;

        for _ in 0..MAX_AUTO_DEREF {
            let Some(record) = self.record_of(current) else {
                break;
            };
            if !seen.insert(record) {
                break;
            }
            paths.push(RecordPath { record });
            let Some(step) = self.deref_step(record, mutable, span)? else {
                break;
            };
            current = step.target;
        }
        Ok(paths)
    }

    pub(super) fn deref_step(
        &mut self,
        record: TypeId,
        mutable: bool,
        span: crate::source::Span,
    ) -> Result<Option<DerefStep>, Diagnostic> {
        let (trait_name, method_name) = if mutable {
            ("DerefMut", "deref_mut")
        } else {
            ("Deref", "deref")
        };
        self.instantiate_generic_impl(record)?;
        let keys: Vec<_> = self
            .impls
            .get(&record)
            .into_iter()
            .flat_map(|info| info.trait_methods.keys())
            .filter(|key| key.name == trait_name)
            .cloned()
            .collect();
        if keys.is_empty() {
            return Ok(None);
        }
        if keys.len() != 1 {
            return Err(Diagnostic::new(
                span,
                format!("type has multiple {trait_name} implementations; auto-deref is ambiguous"),
            ));
        }
        let key = &keys[0];
        if key.args.len() != 1 {
            return Err(Diagnostic::new(
                span,
                format!("{trait_name} must have exactly one target type argument"),
            ));
        }
        let target_arg = match key.args[0] {
            super::GenericArgKey::Type(ty) => ty,
            super::GenericArgKey::Int(_) => {
                return Err(Diagnostic::new(
                    span,
                    format!("{trait_name} target must be a type"),
                ));
            }
        };
        let raw = self
            .impls
            .get(&record)
            .and_then(|info| info.trait_methods.get(key))
            .and_then(|set| set.methods.get(method_name))
            .cloned()
            .ok_or_else(|| {
                Diagnostic::new(
                    span,
                    format!("{trait_name} implementation is missing '{method_name}'"),
                )
            })?;
        let TypeKind::Function { params, result, .. } = self.types.kind(raw.ty) else {
            return Err(Diagnostic::new(
                span,
                format!("{trait_name} '{method_name}' must be a function"),
            ));
        };
        let Some(receiver) = params.first().and_then(|&param| self.pointee(param)) else {
            return Err(Diagnostic::new(
                span,
                format!("{trait_name} '{method_name}' must take a pointer receiver"),
            ));
        };
        if receiver.ty != record || receiver.qualifiers.is_const == mutable {
            return Err(Diagnostic::new(
                span,
                if mutable {
                    "DerefMut must take a mutable pointer to its implementing type"
                } else {
                    "Deref must take a const pointer to its implementing type"
                },
            ));
        }
        let Some(target) = self.pointee(*result) else {
            return Err(Diagnostic::new(
                span,
                format!("{trait_name} '{method_name}' must return a pointer"),
            ));
        };
        if target.ty != target_arg {
            return Err(Diagnostic::new(
                span,
                format!("{trait_name} target argument does not match its return pointer"),
            ));
        }
        if target.qualifiers.is_const == mutable {
            return Err(Diagnostic::new(
                span,
                if mutable {
                    "DerefMut must return a mutable pointer"
                } else {
                    "Deref must return a pointer to const"
                },
            ));
        }
        if mutable {
            let deref_keys: Vec<_> = self
                .impls
                .get(&record)
                .into_iter()
                .flat_map(|info| info.trait_methods.keys())
                .filter(|key| key.name == "Deref")
                .cloned()
                .collect();
            if deref_keys.len() != 1 || deref_keys[0].args != key.args {
                return Err(Diagnostic::new(
                    span,
                    "DerefMut requires a matching Deref implementation",
                ));
            }
            let deref_method = self
                .impls
                .get(&record)
                .and_then(|info| info.trait_methods.get(&deref_keys[0]))
                .and_then(|set| set.methods.get("deref"))
                .ok_or_else(|| {
                    Diagnostic::new(span, "DerefMut requires a matching Deref method")
                })?;
            let TypeKind::Function { result, .. } = self.types.kind(deref_method.ty) else {
                return Err(Diagnostic::new(span, "Deref::deref must return a pointer"));
            };
            let Some(deref_target) = self.pointee(*result) else {
                return Err(Diagnostic::new(span, "Deref::deref must return a pointer"));
            };
            if deref_target.ty != target.ty || !deref_target.qualifiers.is_const {
                return Err(Diagnostic::new(
                    span,
                    "DerefMut and Deref must use the same target, with Deref returning const",
                ));
            }
        }
        let method = self
            .trait_method_named(record, key, method_name)?
            .expect("the projection method was checked above");
        Ok(Some(DerefStep {
            name: method_name,
            method,
            target: target.ty,
        }))
    }

    pub(super) fn method_with_autoderef(
        &mut self,
        receiver: TypeId,
        name: &ast::Name,
    ) -> Result<Option<AutoMethod>, Diagnostic> {
        let found = self.find_method_path(receiver, name, false)?;
        let Some(found) = found else {
            return Ok(None);
        };
        if !found.steps.is_empty() && self.method_needs_mut_receiver(&found.method) {
            if let Some(mutable) = self.find_method_path(receiver, name, true)? {
                return Ok(Some(mutable));
            }
            return Err(Diagnostic::new(
                name.span,
                format!("method '{}' needs a DerefMut implementation", name.text),
            ));
        }
        Ok(Some(found))
    }

    fn find_method_path(
        &mut self,
        receiver: TypeId,
        name: &ast::Name,
        mutable: bool,
    ) -> Result<Option<AutoMethod>, Diagnostic> {
        let mut current = receiver;
        let mut steps = Vec::new();
        let mut seen = HashSet::new();
        for _ in 0..MAX_AUTO_DEREF {
            let Some(owner) = self.receiver_of(current) else {
                break;
            };
            if !seen.insert(owner) {
                break;
            }
            if let Some(method) = self.method_of(owner, name)? {
                return Ok(Some(AutoMethod { method, steps }));
            }
            // Only a record can carry a Deref implementation to a further owner.
            let Some(record) = self.record_of(current) else {
                break;
            };
            let Some(step) = self.deref_step(record, mutable, name.span)? else {
                break;
            };
            current = step.target;
            steps.push(step);
        }
        Ok(None)
    }

    fn method_needs_mut_receiver(&self, method: &MethodInfo) -> bool {
        let TypeKind::Function { params, .. } = self.types.kind(method.ty) else {
            return false;
        };
        params
            .first()
            .and_then(|&receiver| self.pointee(receiver))
            .is_some_and(|pointee| !pointee.qualifiers.is_const)
    }
}
