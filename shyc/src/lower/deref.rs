use super::{AutoMethod, DerefStep, Lowerer, MethodInfo, RecordPath};
use crate::ast;
use crate::sema::types::{TypeId, TypeKind};
use crate::source::Diagnostic;
use std::collections::HashSet;

pub(super) const MAX_AUTO_DEREF: usize = 8;

/// The type a generic method was found on, and the `Deref` projections a call
/// must apply before reaching it.
pub(super) struct GenericMethodPath {
    pub owner: TypeId,
    pub steps: Vec<DerefStep>,
}

/// What one receiver lookup found at a type.
enum MethodLookup {
    Concrete(MethodInfo),
    Generic,
}

/// One receiver walk result: where the method was found and the autoderef it
/// took to get there.
struct MethodPath {
    owner: TypeId,
    found: MethodLookup,
    steps: Vec<DerefStep>,
}

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
        let Some(path) = self.find_method_path(receiver, name, false)? else {
            return Ok(None);
        };
        let MethodLookup::Concrete(method) = path.found else {
            return Ok(None);
        };
        if !path.steps.is_empty() && self.method_needs_mut_receiver(&method) {
            if let Some(mutable) = self.find_method_path(receiver, name, true)? {
                if let MethodLookup::Concrete(method) = mutable.found {
                    return Ok(Some(AutoMethod {
                        method,
                        steps: mutable.steps,
                    }));
                }
            }
            return Err(Diagnostic::new(
                name.span,
                format!("method '{}' needs a DerefMut implementation", name.text),
            ));
        }
        Ok(Some(AutoMethod {
            method,
            steps: path.steps,
        }))
    }

    /// The generic method found from `receiver`, with the autoderef a call must
    /// apply. `None` when the first matching method is concrete.
    pub(super) fn generic_method_path(
        &mut self,
        receiver: TypeId,
        name: &ast::Name,
    ) -> Result<Option<GenericMethodPath>, Diagnostic> {
        let Some(path) = self.find_method_path(receiver, name, false)? else {
            return Ok(None);
        };
        if let MethodLookup::Concrete(_) = path.found {
            return Ok(None);
        }
        if !path.steps.is_empty() && self.generic_method_needs_mut_receiver(path.owner, name)? {
            if let Some(mutable) = self.find_method_path(receiver, name, true)?
                && let MethodLookup::Generic = mutable.found
            {
                return Ok(Some(GenericMethodPath {
                    owner: mutable.owner,
                    steps: mutable.steps,
                }));
            }
            return Err(Diagnostic::new(
                name.span,
                format!("method '{}' needs a DerefMut implementation", name.text),
            ));
        }
        Ok(Some(GenericMethodPath {
            owner: path.owner,
            steps: path.steps,
        }))
    }

    /// The first method `name` a receiver reaches.
    ///
    /// The exact type is asked first, so a pointer's own impl wins over its
    /// pointee's and an unrelated pointer impl cannot hide a pointee method.
    /// When the type has no such method, one autoderef step is taken: a raw
    /// pointer moves to its pointee, and a record moves through its `Deref`
    /// (or `DerefMut`) implementation. The walk is bounded and stops if a type
    /// repeats.
    fn find_method_path(
        &mut self,
        receiver: TypeId,
        name: &ast::Name,
        mutable: bool,
    ) -> Result<Option<MethodPath>, Diagnostic> {
        let mut seen = HashSet::new();
        self.find_method_path_at(receiver, name, mutable, &mut seen)
    }

    fn find_method_path_at(
        &mut self,
        current: TypeId,
        name: &ast::Name,
        mutable: bool,
        seen: &mut HashSet<TypeId>,
    ) -> Result<Option<MethodPath>, Diagnostic> {
        if seen.len() >= MAX_AUTO_DEREF || !seen.insert(current) {
            return Ok(None);
        }
        if let Some(method) = self.method_of(current, name)? {
            return Ok(Some(MethodPath {
                owner: current,
                found: MethodLookup::Concrete(method),
                steps: Vec::new(),
            }));
        }
        if self.has_generic_method(current, &name.text)? {
            return Ok(Some(MethodPath {
                owner: current,
                found: MethodLookup::Generic,
                steps: Vec::new(),
            }));
        }
        // A raw pointer dereferences to its pointee. Only a record can carry a
        // `Deref` implementation to a further owner.
        let next = match self.pointee(current) {
            Some(pointee) => pointee.ty,
            None => {
                let Some(record) = self.record_of(current) else {
                    return Ok(None);
                };
                let Some(step) = self.deref_step(record, mutable, name.span)? else {
                    return Ok(None);
                };
                let Some(mut path) =
                    self.find_method_path_at(step.target, name, mutable, seen)?
                else {
                    return Ok(None);
                };
                path.steps.insert(0, step);
                return Ok(Some(path));
            }
        };
        self.find_method_path_at(next, name, mutable, seen)
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

    /// Whether a generic method's own declaration takes a writable `self`.
    fn generic_method_needs_mut_receiver(
        &mut self,
        owner: TypeId,
        name: &ast::Name,
    ) -> Result<bool, Diagnostic> {
        let template = self.generic_method_template(owner, name)?;
        let Some(ast::DerivedDeclarator::Function(function)) =
            template.decl.declarator.derived.first()
        else {
            return Ok(false);
        };
        let Some(receiver) = function.params.first() else {
            return Ok(false);
        };
        Ok(!receiver.specs.qualifiers.contains(&ast::Qualifier::Const))
    }
}
