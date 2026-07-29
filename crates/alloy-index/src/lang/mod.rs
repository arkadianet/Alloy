//! Language backends (RFC-0014). Rust-only by design: no dynamic loading,
//! no `alloy-lang-*` crates, no `cdylib` (V2 §16, ADR F-15, rule RS9).

pub mod rust;
