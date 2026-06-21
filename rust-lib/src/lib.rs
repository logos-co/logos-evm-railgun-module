//! `railgun_module` — wraps the RAILGUN (`railgun-rs`) shielded-pool engine as a
//! Logos module, giving the EVM wallet private transactions (shield / private
//! transfer / unshield) over the same module bus as keystore / eth-rpc / uniswap.
//!
//! Authoring split (mirrors `uniswap_module`): the pure, cargo-testable cores
//! (`rpc_backend`, `keys`, `db_adapter`, `engine`) carry no Logos dependency and
//! are tested with `cargo test --no-default-features`. The `glue` module (behind
//! the default `logos_module` feature) wires the contract trait to the Logos
//! runtime via `logos-rust-sdk`.
//!
//! ⚠️ The upstream engine is explicitly **unaudited / not production-ready**. This
//! module is Sepolia-first; mainnet is gated behind an explicit opt-in.

pub mod db_adapter;
pub mod engine;
pub mod keys;
pub mod rpc_backend;

#[cfg(feature = "logos_module")]
mod glue;

pub use rpc_backend::{EthRpcEip1193, RpcBackend};
