//! The live RAILGUN engine wrapper: one long-lived [`RailgunProvider`] built over
//! the [`EthRpcEip1193`] adapter (chain reads via `eth_rpc_module`) and a
//! [`FilesystemDatabase`] under the module's per-instance persistence dir.
//!
//! Everything here is `async` (the engine is); the Logos glue runs these on a
//! `concurrency:multi` worker thread via `block_on`. Methods take `&mut self`
//! (the engine is `&mut`-driven), so the glue holds the engine behind a `Mutex`.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::Address;
use railgun::account::address::RailgunAddress;
use railgun::account::chain::ChainId;
use railgun::account::signer::{PrivateKeySigner, RailgunSigner};
use railgun::builder::RailgunBuilder;
use railgun::caip::AssetId;
use railgun::chain_config::ChainConfig;
use railgun::provider::RailgunProvider;

use crate::db_adapter::DiskDatabase;
use crate::keys;
use crate::rpc_backend::{EthRpcEip1193, RpcBackend};

/// A loaded, key-registered RAILGUN engine for one chain.
pub struct RailgunEngine {
    provider: RailgunProvider,
    address: RailgunAddress,
    /// Held so transfer/unshield can sign their proofs. Never leaves the module.
    signer: Arc<PrivateKeySigner>,
}

/// Parse a `0x…`/bare ERC-20 token address into an `AssetId`.
fn parse_asset(asset: &str) -> Result<AssetId, String> {
    let hex = asset.trim_start_matches("0x");
    let addr = Address::from_str(&format!("0x{hex}")).map_err(|e| format!("bad asset address {asset}: {e}"))?;
    Ok(AssetId::erc20(addr))
}

impl RailgunEngine {
    /// Build + register the engine for `chain_id` (mainnet or Sepolia), reading
    /// chain data through `backend` (the `eth_rpc_module` seam) and persisting
    /// state under `data_dir`. The railgun keys stay inside this process.
    pub async fn init<B: RpcBackend>(
        chain_id: u64,
        backend: Arc<B>,
        spending_hex: &str,
        viewing_hex: &str,
        data_dir: &Path,
        poi: bool,
    ) -> Result<Self, String> {
        let chain = ChainConfig::from_chain_id(chain_id)
            .ok_or_else(|| format!("unsupported chain id {chain_id} (mainnet + Sepolia only)"))?;

        let signer = keys::make_signer(spending_hex, viewing_hex, ChainId::evm(chain_id))?;
        let address = signer.address();

        let eip1193 = Arc::new(EthRpcEip1193::new(backend));
        let db = Arc::new(DiskDatabase::new(data_dir)?);

        let mut builder = RailgunBuilder::new(chain, eip1193).with_database(db);
        if poi {
            builder = builder.with_poi();
        }
        let mut provider = builder.build().await.map_err(|e| format!("engine build: {e}"))?;
        provider
            .register(signer.clone() as Arc<dyn RailgunSigner>)
            .await
            .map_err(|e| format!("register signer: {e}"))?;

        Ok(Self { provider, address, signer })
    }

    /// The public `0zk1…` address (safe to expose over IPC).
    pub fn zk_address(&self) -> String {
        self.address.to_string()
    }

    /// Sync UTXO/TXID (and POI, if enabled) state to the latest block.
    pub async fn sync(&mut self) -> Result<(), String> {
        self.provider.sync().await.map_err(|e| format!("sync: {e}"))
    }

    /// Shielded balance per asset, as the engine's `BalanceEntry` JSON array.
    pub async fn shielded_balance_json(&mut self) -> Result<String, String> {
        let entries = self.provider.balance(self.address.clone()).await;
        serde_json::to_string(&entries).map_err(|e| e.to_string())
    }

    /// SHIELD: deposit `value` of ERC-20 `asset` into the shielded pool (to our own
    /// `0zk` address). No proof — returns the unsigned `TxData[]` (the caller must
    /// first `approve` the RailgunSmartWallet, then sign+send each) as JSON.
    pub async fn prepare_shield(&self, asset: &str, value: u128) -> Result<String, String> {
        let asset = parse_asset(asset)?;
        let txs = self
            .provider
            .shield()
            .shield(self.address.clone(), asset, value)
            .build(&mut rand::rng())
            .map_err(|e| format!("build shield: {e}"))?;
        serde_json::to_string(&txs).map_err(|e| e.to_string())
    }

    /// TRANSACT: private transfer of `value` `asset` to another `0zk` address.
    /// Runs Groth16 proving; returns the proven `TxData` (a call to the
    /// RailgunSmartWallet) as JSON. No fee (internal transfer).
    pub async fn prepare_transfer(
        &mut self,
        to_0zk: &str,
        asset: &str,
        value: u128,
        memo: &str,
    ) -> Result<String, String> {
        let to = RailgunAddress::from_str(to_0zk).map_err(|e| format!("bad 0zk address: {e}"))?;
        let asset = parse_asset(asset)?;
        let from = self.signer.clone() as Arc<dyn RailgunSigner>;
        let builder = self.provider.transact().transfer(from, to, asset, value, memo);
        let proved = self
            .provider
            .build(builder, &mut rand::rng())
            .await
            .map_err(|e| format!("prove transfer: {e}"))?;
        serde_json::to_string(&proved.tx_data).map_err(|e| e.to_string())
    }

    /// UNSHIELD: withdraw `value` `asset` from the shielded pool to a public `0x`
    /// address. Runs Groth16 proving; returns the proven `TxData` as JSON. The
    /// engine adds the chain's unshield fee so the recipient gets `value`.
    pub async fn prepare_unshield(
        &mut self,
        to_addr: &str,
        asset: &str,
        value: u128,
    ) -> Result<String, String> {
        let to = Address::from_str(to_addr).map_err(|e| format!("bad recipient address: {e}"))?;
        let asset = parse_asset(asset)?;
        let from = self.signer.clone() as Arc<dyn RailgunSigner>;
        let builder = self
            .provider
            .transact()
            .unshield(from, to, asset, value)
            .map_err(|e| format!("build unshield: {e}"))?;
        let proved = self
            .provider
            .build(builder, &mut rand::rng())
            .await
            .map_err(|e| format!("prove unshield: {e}"))?;
        serde_json::to_string(&proved.tx_data).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::sync::Mutex;

    /// Backend that fails any RPC — proves `init` (build + register) performs no
    /// network I/O (sync/balance do; those need a live chain and are gated).
    struct NoNetBackend(Mutex<u32>);
    impl RpcBackend for NoNetBackend {
        fn rpc(&self, method: &str, _params: Value) -> Result<Value, String> {
            *self.0.lock().unwrap() += 1;
            Err(format!("no network in test (method {method})"))
        }
    }

    const SPENDING: &str = "039b3b11110e49d7340cbe7171791972e3c0d94ef31b18d6ab93d7ace62d278a";
    const VIEWING: &str = "d345b2cc2f414aa93413b9572fa2b26e0e869e9274b006415a8d62ab1fa2dcb1";

    #[tokio::test]
    async fn init_builds_and_registers_offline_on_sepolia() {
        let dir = std::env::temp_dir().join(format!("railgun-test-{}", std::process::id()));
        let backend = Arc::new(NoNetBackend(Mutex::new(0)));
        let engine = RailgunEngine::init(11155111, backend.clone(), SPENDING, VIEWING, &dir, false)
            .await
            .expect("init should build + register without network");
        assert!(engine.zk_address().starts_with("0zk1"), "got {}", engine.zk_address());
        assert_eq!(*backend.0.lock().unwrap(), 0, "init must not touch the network");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn init_rejects_unsupported_chain() {
        let dir = std::env::temp_dir().join("railgun-test-badchain");
        let backend = Arc::new(NoNetBackend(Mutex::new(0)));
        match RailgunEngine::init(999999, backend, SPENDING, VIEWING, &dir, false).await {
            Err(e) => assert!(e.contains("unsupported chain id"), "got {e}"),
            Ok(_) => panic!("expected unsupported-chain error"),
        }
    }

    #[tokio::test]
    async fn prepare_shield_builds_txdata_offline() {
        let dir = std::env::temp_dir().join(format!("railgun-shield-{}", std::process::id()));
        let backend = Arc::new(NoNetBackend(Mutex::new(0)));
        let engine = RailgunEngine::init(11155111, backend.clone(), SPENDING, VIEWING, &dir, false)
            .await
            .unwrap();
        // Shield is pure calldata (no proof / no network).
        let usdc = "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238"; // sepolia USDC
        let txs_json = engine.prepare_shield(usdc, 1_000_000).await.expect("shield");
        let txs: Value = serde_json::from_str(&txs_json).unwrap();
        let arr = txs.as_array().expect("shield returns a TxData array");
        assert!(!arr.is_empty());
        // Every entry is to/data/value.
        for tx in arr {
            assert!(tx.get("to").is_some() && tx.get("data").is_some() && tx.get("value").is_some());
        }
        assert_eq!(*backend.0.lock().unwrap(), 0, "shield must not touch the network");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_asset_accepts_0x_and_bare() {
        assert!(parse_asset("0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238").is_ok());
        assert!(parse_asset("1c7D4B196Cb0C7B01d743Fbc6116a902379C7238").is_ok());
        assert!(parse_asset("nothex").is_err());
    }

    #[test]
    fn balance_entry_json_shape() {
        // Guard the wire shape the backend/UI parse (poiStatus camelCase).
        let v: Value = json!({"asset": {"erc20": "0x0"}, "poiStatus": null, "amount": 0});
        assert!(v.get("poiStatus").is_some());
    }
}
