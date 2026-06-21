//! Logos module glue for `railgun_module` (rust-first authoring).
//!
//! Wires the contract trait to the Logos runtime: the engine's chain reads are
//! served by `modules().eth_rpc_module` (declared in metadata.json
//! `dependencies`) through the [`EthRpcBackend`] → [`EthRpcEip1193`] adapter.
//!
//! `concurrency: "multi"` (metadata.json): proof generation and sync are
//! CPU/network-heavy and blocking, so the module opts into concurrent dispatch —
//! a long proof runs on a worker thread instead of freezing the module. The
//! engine itself is a single `&mut`-driven object, so engine access serializes
//! behind a `futures::lock::Mutex`; concurrency:multi keeps the *dispatch* free
//! while that work runs.
//!
//! ## Async bridge
//! The engine is `async`; each (sync) glue method drives it on a **per-call
//! current-thread** tokio runtime via `block_on`, so the engine's outbound
//! `modules().eth_rpc_module` IPC executes on this dispatch worker thread — which
//! carries the Qt event loop needed for the QtRO round-trip under multi-dispatch.
//!
//! ⚠️ Unaudited upstream engine — Sepolia-first; the railgun keys never leave
//! this module (see [`crate::keys`]).

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use futures::lock::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::engine::RailgunEngine;
use crate::rpc_backend::RpcBackend;

pub trait RailgunModule: Send + Sync + 'static {
    /// One-time load: `{ "chainId": u64, "spendingKey": hex, "viewingKey": hex,
    /// "poi": bool }`. Imports the railgun keys (held in-module), builds the
    /// engine for the chain, and returns `{ ok, address }` (the `0zk` address).
    fn init(&self, params_json: String) -> String;
    /// The public `0zk1…` RAILGUN address (`{ ok, address }`).
    fn get_zk_address(&self) -> String;
    /// Sync UTXO/TXID (and POI, if enabled) state to the latest block.
    fn sync(&self) -> String;
    /// Shielded balance per asset (the engine's `BalanceEntry` JSON array).
    fn get_shielded_balance(&self) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct RailgunModuleImpl {
    persist_dir: RwLock<Option<PathBuf>>,
    engine: Mutex<Option<RailgunEngine>>,
}

// ── eth_rpc-backed RpcBackend (the chain-read seam the engine adapter uses) ──

/// Backs the engine's `Eip1193Provider` by forwarding raw JSON-RPC to
/// `modules().eth_rpc_module.raw_rpc(chainId, method, params)` and unwrapping the
/// `{ ok, result }` envelope.
struct EthRpcBackend {
    chain_id: i64,
}

impl RpcBackend for EthRpcBackend {
    fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let resp = modules()
            .eth_rpc_module
            .raw_rpc(self.chain_id, method.to_string(), params.to_string())
            .map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(v.get("error").and_then(Value::as_str).unwrap_or("eth_rpc failed").to_string());
        }
        v.get("result").cloned().ok_or_else(|| "eth_rpc: missing result".to_string())
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

/// Drive an async engine op on a per-call current-thread runtime so its outbound
/// `modules()` IPC runs on this (event-loop-carrying) dispatch worker thread.
fn block_on<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime")
        .block_on(f)
}

#[derive(Deserialize)]
struct InitParams {
    #[serde(rename = "chainId")]
    chain_id: u64,
    #[serde(rename = "spendingKey")]
    spending_key: String,
    #[serde(rename = "viewingKey")]
    viewing_key: String,
    #[serde(default)]
    poi: bool,
}

impl RailgunModuleImpl {
    fn persist_dir(&self) -> Result<PathBuf, String> {
        self.persist_dir
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| "railgun_module not initialized (context not ready)".to_string())
    }
}

impl RailgunModule for RailgunModuleImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        *self.persist_dir.write().unwrap() = Some(PathBuf::from(&ctx.instance_persistence_path));
    }

    fn init(&self, params_json: String) -> String {
        let p: InitParams = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(format!("bad init params: {e}")),
        };
        let dir = match self.persist_dir() {
            Ok(d) => d.join(format!("chain-{}", p.chain_id)),
            Err(e) => return err(e),
        };
        let backend = Arc::new(EthRpcBackend { chain_id: p.chain_id as i64 });

        let res = block_on(async {
            let engine = RailgunEngine::init(
                p.chain_id,
                backend,
                &p.spending_key,
                &p.viewing_key,
                &dir,
                p.poi,
            )
            .await?;
            let addr = engine.zk_address();
            *self.engine.lock().await = Some(engine);
            Ok::<String, String>(addr)
        });

        match res {
            Ok(addr) => json!({ "ok": true, "address": addr }).to_string(),
            Err(e) => err(e),
        }
    }

    fn get_zk_address(&self) -> String {
        block_on(async {
            match self.engine.lock().await.as_ref() {
                Some(e) => json!({ "ok": true, "address": e.zk_address() }).to_string(),
                None => err("railgun_module not initialized (call init first)"),
            }
        })
    }

    fn sync(&self) -> String {
        block_on(async {
            match self.engine.lock().await.as_mut() {
                Some(e) => match e.sync().await {
                    Ok(()) => json!({ "ok": true }).to_string(),
                    Err(e) => err(e),
                },
                None => err("railgun_module not initialized (call init first)"),
            }
        })
    }

    fn get_shielded_balance(&self) -> String {
        block_on(async {
            match self.engine.lock().await.as_mut() {
                Some(e) => match e.shielded_balance_json().await {
                    Ok(j) => json!({ "ok": true, "balances": serde_json::from_str::<Value>(&j).unwrap_or(Value::Null) }).to_string(),
                    Err(e) => err(e),
                },
                None => err("railgun_module not initialized (call init first)"),
            }
        })
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<RailgunModuleImpl>();
}
