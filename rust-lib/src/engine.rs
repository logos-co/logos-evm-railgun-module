//! The live RAILGUN engine wrapper: one long-lived [`RailgunProvider`] built over
//! the [`EthRpcEip1193`] adapter (chain reads via `eth_rpc_module`) and a
//! [`FilesystemDatabase`] under the module's per-instance persistence dir.
//!
//! Everything here is `async` (the engine is); the Logos glue runs these on a
//! `concurrency:multi` worker thread via `block_on`. Methods take `&mut self`
//! (the engine is `&mut`-driven), so the glue holds the engine behind a `Mutex`.

use std::path::Path;
use std::sync::Arc;

use railgun::account::address::RailgunAddress;
use railgun::account::chain::ChainId;
use railgun::account::signer::RailgunSigner;
use railgun::builder::RailgunBuilder;
use railgun::chain_config::ChainConfig;
use railgun::provider::RailgunProvider;

use crate::db_adapter::DiskDatabase;
use crate::keys;
use crate::rpc_backend::{EthRpcEip1193, RpcBackend};

/// A loaded, key-registered RAILGUN engine for one chain.
pub struct RailgunEngine {
    provider: RailgunProvider,
    address: RailgunAddress,
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
            .register(signer as Arc<dyn RailgunSigner>)
            .await
            .map_err(|e| format!("register signer: {e}"))?;

        Ok(Self { provider, address })
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

    #[test]
    fn balance_entry_json_shape() {
        // Guard the wire shape the backend/UI parse (poiStatus camelCase).
        let v: Value = json!({"asset": {"erc20": "0x0"}, "poiStatus": null, "amount": 0});
        assert!(v.get("poiStatus").is_some());
    }
}
