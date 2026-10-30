//! Shared ABI planning surface for ShyC backends and callers.
//!
//! The implementation lives in the independent `abi-plan` crate so the
//! simulator/dynamic-library project can use the same plans. ShyC re-exports
//! the model here while its backend migration is in progress.
pub use abi_plan::{
    AbiError, AbiField, AbiKind, AbiType, ArgPlan, CallPlan, DataLayout, FunctionSignature,
    Location, Passing, RegisterClass, RegisterId, RegisterUse, ReturnPlan, StackPlan, ValuePart,
    plan,
};

/// Name used by the compiler when it needs the project's own calling
/// convention rather than a host C ABI.
pub type TargetAbi = AbiKind;
