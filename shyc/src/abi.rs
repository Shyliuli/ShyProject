//! The ABI model ShyC's lowering and backends share.
//!
//! The implementation lives in the independent `abi-plan` crate so the
//! simulator/dynamic-library project can use the same plans.
pub use abi_plan::{
    AbiError, AbiField, AbiKind, AbiType, ArgPlan, CallPlan, DataLayout, FunctionSignature,
    Location, Passing, RegisterClass, RegisterId, RegisterUse, ReturnPlan, StackPlan, ValuePart,
    plan,
};
