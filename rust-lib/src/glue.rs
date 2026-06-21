//! Logos module glue for `railgun_module` (rust-first authoring).
//!
//! Wires the contract trait to the Logos runtime: the engine's chain reads are
//! served by `modules().eth_rpc_module` (declared in metadata.json
//! `dependencies`) through the [`EthRpcBackend`] → [`EthRpcEip1193`] adapter.
//!
//! ## Concurrency
//! `concurrency: "single"` for now. The railgun engine (`RailgunProvider`) is a
//! single `&mut`-driven object and is **not `Send + Sync`** — its `dyn
//! RailgunSigner` field isn't, so it can't satisfy multi-dispatch's `Send + Sync`
//! bound. So we hold it directly behind `&mut self`. The cost: a long proof
//! blocks the module's dispatch. `concurrency: "multi"` is a follow-up that needs
//! a one-line upstream patch (`RailgunSigner: Send + Sync`, satisfied by the
//! concrete `PrivateKeySigner`) carried in our engine fork.
//!
//! ## Async bridge
//! The engine is `async`; each (sync) glue method drives it on a per-call
//! current-thread tokio runtime via `block_on`, on the module's dispatch thread
//! (which carries the Qt event loop the engine's outbound `modules()` IPC needs).
//!
//! ⚠️ Unaudited upstream engine — Sepolia-first; the railgun keys never leave
//! this module (see [`crate::keys`]).

use std::future::Future;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::{Address, ChainId, Signature, B256};
use alloy::signers::{Error as SignerError, Result as SignerResult, Signer};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::engine::RailgunEngine;
use crate::rpc_backend::RpcBackend;

pub trait RailgunModule: 'static {
    /// One-time load: `{ "chainId": u64, "spendingKey": hex, "viewingKey": hex,
    /// "poi": bool }`. Imports the railgun keys (held in-module), builds the
    /// engine for the chain, and returns `{ ok, address }` (the `0zk` address).
    fn init(&mut self, params_json: String) -> String;
    /// The public `0zk1…` RAILGUN address (`{ ok, address }`).
    fn get_zk_address(&mut self) -> String;
    /// Sync UTXO/TXID (and POI, if enabled) state to the latest block.
    fn sync(&mut self) -> String;
    /// Shielded balance per asset (`{ ok, balances: [BalanceEntry] }`).
    fn get_shielded_balance(&mut self) -> String;
    /// SHIELD (deposit public → private): `{ "asset": "0x…", "amount": "decimal" }`
    /// → `{ ok, txs: [TxData] }` for the caller to approve+sign+send. No proof.
    fn prepare_shield(&mut self, params_json: String) -> String;
    /// Private TRANSFER (0zk → 0zk): `{ "to": "0zk…", "asset", "amount", "memo"? }`
    /// → `{ ok, tx: TxData }` (Groth16-proven). No fee.
    fn prepare_transfer(&mut self, params_json: String) -> String;
    /// UNSHIELD (private → public 0x): `{ "to": "0x…", "asset", "amount" }`
    /// → `{ ok, tx: TxData }` (Groth16-proven; engine adds the unshield fee).
    fn prepare_unshield(&mut self, params_json: String) -> String;
    /// RELAYED private send (ERC-4337 — hides the sender): `{ "to": "0zk…"|"0x…",
    /// "asset", "amount", "memo"?, "owner": "0x…", "bundlerUrl": "https://…" }`
    /// → `{ ok, userOpHash }`. Routes 0zk→transfer / 0x→unshield, wraps it in a 7702
    /// UserOp paid from the shielded pool, signs it with `owner`'s key **via
    /// keystore** (`sign_digest`, key never leaves keystore), and submits the op to
    /// `bundlerUrl` **through eth_rpc** (`raw_rpc_url`, proxied — the bundler never
    /// sees the user's IP). Needs a live bundler + chain (no offline path).
    fn relayed_send(&mut self, params_json: String) -> String;

    fn on_context_ready(&mut self, _ctx: &RustModuleContext) {}
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct RailgunModuleImpl {
    persist_dir: Option<PathBuf>,
    engine: Option<RailgunEngine>,
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
            .raw_rpc(self.chain_id, method, &params.to_string())
            .map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(v.get("error").and_then(Value::as_str).unwrap_or("eth_rpc failed").to_string());
        }
        v.get("result").cloned().ok_or_else(|| "eth_rpc: missing result".to_string())
    }
}

// ── keystore-bridge signer (the EOA owner of the 7702 smart account) ─────────

/// An alloy [`Signer`] that signs by calling `modules().keystore_module.sign_digest`
/// over IPC, so the EOA private key never enters this module. Used by
/// [`SignableUserOperation::sign`](userop_kit::signable_user_operation::SignableUserOperation)
/// to sign the relayer's userOp hash (and its 7702 authorization hash) — both
/// raw 32-byte digests the keystore signs without an EIP-191/712 prefix.
struct KeystoreBridgeSigner {
    owner: Address,
    chain_id: u64,
}

#[async_trait]
impl Signer for KeystoreBridgeSigner {
    async fn sign_hash(&self, hash: &B256) -> SignerResult<Signature> {
        let resp = modules()
            .keystore_module
            .sign_digest(&self.owner.to_string(), &format!("0x{hash:x}"))
            .map_err(|e| SignerError::other(e.to_string()))?;
        let v: Value = serde_json::from_str(&resp).map_err(|e| SignerError::other(e.to_string()))?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            let msg = v.get("error").and_then(Value::as_str).unwrap_or("sign_digest failed");
            return Err(SignerError::other(msg.to_string()));
        }
        let sig_hex = v
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(|| SignerError::other("sign_digest: no signature"))?;
        let bytes = hex::decode(sig_hex.trim_start_matches("0x"))
            .map_err(|e| SignerError::other(e.to_string()))?;
        Signature::try_from(bytes.as_slice()).map_err(|e| SignerError::other(e.to_string()))
    }

    fn address(&self) -> Address {
        self.owner
    }
    fn chain_id(&self) -> Option<ChainId> {
        Some(self.chain_id)
    }
    fn set_chain_id(&mut self, chain_id: Option<ChainId>) {
        if let Some(c) = chain_id {
            self.chain_id = c;
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

/// Drive an async engine op on a per-call current-thread runtime (on this
/// dispatch thread, so the engine's outbound `modules()` IPC has the event loop).
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

// Amounts are decimal strings (u128 wei exceeds JSON's safe-integer range).
#[derive(Deserialize)]
struct ShieldParams {
    asset: String,
    amount: String,
}
#[derive(Deserialize)]
struct TransferParams {
    to: String,
    asset: String,
    amount: String,
    #[serde(default)]
    memo: String,
}
#[derive(Deserialize)]
struct UnshieldParams {
    to: String,
    asset: String,
    amount: String,
}
#[derive(Deserialize)]
struct RelayedSendParams {
    to: String,
    asset: String,
    amount: String,
    #[serde(default)]
    memo: String,
    owner: String,
    #[serde(rename = "bundlerUrl")]
    bundler_url: String,
}

fn parse_amount(s: &str) -> Result<u128, String> {
    s.parse::<u128>().map_err(|e| format!("bad amount {s:?}: {e}"))
}

impl RailgunModule for RailgunModuleImpl {
    fn on_context_ready(&mut self, ctx: &RustModuleContext) {
        self.persist_dir = Some(PathBuf::from(&ctx.instance_persistence_path));
    }

    fn init(&mut self, params_json: String) -> String {
        let p: InitParams = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(format!("bad init params: {e}")),
        };
        let dir = match &self.persist_dir {
            Some(d) => d.join(format!("chain-{}", p.chain_id)),
            None => return err("railgun_module not initialized (context not ready)"),
        };
        let backend = Arc::new(EthRpcBackend { chain_id: p.chain_id as i64 });

        match block_on(RailgunEngine::init(
            p.chain_id,
            backend,
            &p.spending_key,
            &p.viewing_key,
            &dir,
            p.poi,
        )) {
            Ok(engine) => {
                let addr = engine.zk_address();
                self.engine = Some(engine);
                json!({ "ok": true, "address": addr }).to_string()
            }
            Err(e) => err(e),
        }
    }

    fn get_zk_address(&mut self) -> String {
        match &self.engine {
            Some(e) => json!({ "ok": true, "address": e.zk_address() }).to_string(),
            None => err("railgun_module not initialized (call init first)"),
        }
    }

    fn sync(&mut self) -> String {
        match self.engine.as_mut() {
            Some(e) => match block_on(e.sync()) {
                Ok(()) => json!({ "ok": true }).to_string(),
                Err(e) => err(e),
            },
            None => err("railgun_module not initialized (call init first)"),
        }
    }

    fn get_shielded_balance(&mut self) -> String {
        match self.engine.as_mut() {
            Some(e) => match block_on(e.shielded_balance_json()) {
                Ok(j) => json!({
                    "ok": true,
                    "balances": serde_json::from_str::<Value>(&j).unwrap_or(Value::Null)
                })
                .to_string(),
                Err(e) => err(e),
            },
            None => err("railgun_module not initialized (call init first)"),
        }
    }

    fn prepare_shield(&mut self, params_json: String) -> String {
        let p: ShieldParams = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(format!("bad shield params: {e}")),
        };
        let amount = match parse_amount(&p.amount) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        match self.engine.as_ref() {
            Some(e) => match block_on(e.prepare_shield(&p.asset, amount)) {
                Ok(txs) => json!({
                    "ok": true,
                    "txs": serde_json::from_str::<Value>(&txs).unwrap_or(Value::Null)
                })
                .to_string(),
                Err(e) => err(e),
            },
            None => err("railgun_module not initialized (call init first)"),
        }
    }

    fn prepare_transfer(&mut self, params_json: String) -> String {
        let p: TransferParams = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(format!("bad transfer params: {e}")),
        };
        let amount = match parse_amount(&p.amount) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        match self.engine.as_mut() {
            Some(e) => match block_on(e.prepare_transfer(&p.to, &p.asset, amount, &p.memo)) {
                Ok(tx) => json!({ "ok": true, "tx": serde_json::from_str::<Value>(&tx).unwrap_or(Value::Null) }).to_string(),
                Err(e) => err(e),
            },
            None => err("railgun_module not initialized (call init first)"),
        }
    }

    fn prepare_unshield(&mut self, params_json: String) -> String {
        let p: UnshieldParams = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(format!("bad unshield params: {e}")),
        };
        let amount = match parse_amount(&p.amount) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        match self.engine.as_mut() {
            Some(e) => match block_on(e.prepare_unshield(&p.to, &p.asset, amount)) {
                Ok(tx) => json!({ "ok": true, "tx": serde_json::from_str::<Value>(&tx).unwrap_or(Value::Null) }).to_string(),
                Err(e) => err(e),
            },
            None => err("railgun_module not initialized (call init first)"),
        }
    }

    fn relayed_send(&mut self, params_json: String) -> String {
        let p: RelayedSendParams = match serde_json::from_str(&params_json) {
            Ok(p) => p,
            Err(e) => return err(format!("bad relayed-send params: {e}")),
        };
        let amount = match parse_amount(&p.amount) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let owner = match Address::from_str(&p.owner) {
            Ok(a) => a,
            Err(e) => return err(format!("bad owner address: {e}")),
        };
        let engine = match self.engine.as_mut() {
            Some(e) => e,
            None => return err("railgun_module not initialized (call init first)"),
        };
        let chain_id = engine.chain_id() as i64;

        // 1) Prepare the unsigned 7702 UserOperation (iterates against the bundler).
        let signable = match block_on(engine.prepare_relayed_userop(
            &p.to,
            &p.asset,
            amount,
            &p.memo,
            &p.owner,
            &p.bundler_url,
        )) {
            Ok(s) => s,
            Err(e) => return err(e),
        };

        // 2) Sign it (userOp hash + 7702 auth hash) with the owner's key via keystore.
        let bridge = KeystoreBridgeSigner { owner, chain_id: chain_id as u64 };
        let signed = match block_on(signable.sign(&bridge)) {
            Ok(s) => s,
            Err(e) => return err(format!("sign userop: {e}")),
        };

        // 3) Submit to the bundler through eth_rpc (proxied) — params are the
        //    `eth_sendUserOperation` tuple `[userOp, entryPoint]`.
        let params = match serde_json::to_string(&(&signed.user_op, &signed.entry_point)) {
            Ok(s) => s,
            Err(e) => return err(format!("encode userop: {e}")),
        };
        let resp = match modules().eth_rpc_module.raw_rpc_url(
            chain_id,
            &p.bundler_url,
            "eth_sendUserOperation",
            &params,
        ) {
            Ok(r) => r,
            Err(e) => return err(format!("bundler submit: {e}")),
        };
        let v: Value = serde_json::from_str(&resp).unwrap_or(Value::Null);
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            return err(v.get("error").and_then(Value::as_str).unwrap_or("bundler submit failed"));
        }
        json!({ "ok": true, "userOpHash": v.get("result").cloned().unwrap_or(Value::Null) }).to_string()
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<RailgunModuleImpl>();
}
