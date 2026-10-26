//! Benchmarks, budget checks, and soak tests for the hot paths in `kattri-client-core` and
//! `kattri-input`.
//!
//! The library holds only the budget constants in [`budgets`]. The checks live in
//! `tests/budgets.rs`, the soak loop in `tests/soak.rs`, and the criterion benchmarks in
//! `benches/hot_paths.rs`.

pub mod budgets;
