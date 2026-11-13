//! pe-logic (WS-logic, @iter-bench). Logic: scripts compiled by Cranelift with metering injected into the module, run
//! as systems over the store.
//!
//! Specification: docs/spec/logic.md. v0 so far:
//! - [`abi`]: the guest ABI v0, its host functions and required exports (3.1, 3.4);
//! - [`meter`]: the validator and the injected fuel and depth pass (5.2, 5.3), with the optional NaN
//!   canonicalization at observation points (5.2b);
//! - [`runtime`]: the wasmtime engine of 5.1, a script's compiled module, instances with the per-frame reset and
//!   deterministic faults (3.6);
//! - [`manifest`]: the manifest decoded and checked against the store's layout rules (3.2);
//! - [`build`]: the build service, v0 (4.2);
//! - [`run`]: the batch runner: scripts as systems over the store, faults reported for the caller to keep (3.3,
//!   3.5, 3.6).
//!
//! Still to come in v0: faults kept as world state (with WS-world) and
//! activation inside the window (6.2), and the backends of `code.check` and `code.commit`.

pub mod abi;
pub mod build;
pub mod manifest;
pub mod meter;
pub mod run;
pub mod runtime;

/// The engine settings of 5.1; `Settings::thread_stack` is the native stack a thread that runs guests needs.
pub use runtime::Settings;
