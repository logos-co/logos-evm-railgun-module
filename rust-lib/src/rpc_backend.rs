//! Bridge from the RAILGUN engine's async [`Eip1193Provider`] to the Logos
//! `eth_rpc_module` over a small synchronous [`RpcBackend`] seam.
//!
//! The engine drives all chain reads (chainId / blockNumber / logs / eth_call /
//! estimateGas / gasPrice / transactionCount) through `Eip1193Provider`. We keep
//! the translation pure by abstracting the actual JSON-RPC transport behind
//! [`RpcBackend`]: the Logos glue implements it with
//! `modules().eth_rpc_module.raw_rpc(chain_id, method, params)`, while tests use
//! [`MockBackend`]. This lets the whole adapter be exercised with `cargo test`.

use std::sync::Arc;

use alloy::primitives::{Address, Bytes, FixedBytes};
use async_trait::async_trait;
use eip_1193_provider::provider::{Eip1193Error, Eip1193Provider, RawLog};
use serde_json::{json, Value};

/// Synchronous JSON-RPC transport. One call = one `eth_*` request; returns the
/// `result` value or an error message. The Logos glue backs this with the
/// `eth_rpc_module`; tests back it with [`MockBackend`].
pub trait RpcBackend: Send + Sync + 'static {
    fn rpc(&self, method: &str, params: Value) -> Result<Value, String>;
}

/// Adapts an [`RpcBackend`] to the engine's [`Eip1193Provider`]. Wrap in `Arc`
/// (the engine's `IntoEip1193Provider` is implemented for `Arc<T>`).
pub struct EthRpcEip1193<B: RpcBackend> {
    backend: Arc<B>,
}

impl<B: RpcBackend> EthRpcEip1193<B> {
    pub fn new(backend: Arc<B>) -> Self {
        Self { backend }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, Eip1193Error> {
        self.backend.rpc(method, params).map_err(Eip1193Error::Rpc)
    }
}

// ── hex helpers (JSON-RPC quantities/data are 0x-hex strings) ────────────────

fn as_str(v: &Value) -> Result<&str, Eip1193Error> {
    v.as_str()
        .ok_or_else(|| Eip1193Error::Rpc(format!("expected hex string, got {v}")))
}

fn u64_hex(v: &Value) -> Result<u64, Eip1193Error> {
    let s = as_str(v)?.trim_start_matches("0x");
    u64::from_str_radix(s, 16).map_err(|e| Eip1193Error::Rpc(e.to_string()))
}

fn u128_hex(v: &Value) -> Result<u128, Eip1193Error> {
    let s = as_str(v)?.trim_start_matches("0x");
    u128::from_str_radix(s, 16).map_err(|e| Eip1193Error::Rpc(e.to_string()))
}

fn bytes_hex(v: &Value) -> Result<Bytes, Eip1193Error> {
    let s = as_str(v)?.trim_start_matches("0x");
    let raw = hex::decode(s).map_err(|e| Eip1193Error::Rpc(e.to_string()))?;
    Ok(Bytes::from(raw))
}

fn b32_hex(s: &str) -> Result<FixedBytes<32>, Eip1193Error> {
    let raw = hex::decode(s.trim_start_matches("0x")).map_err(|e| Eip1193Error::Rpc(e.to_string()))?;
    if raw.len() != 32 {
        return Err(Eip1193Error::Rpc(format!("expected 32-byte hash, got {} bytes", raw.len())));
    }
    Ok(FixedBytes::<32>::from_slice(&raw))
}

fn addr_hex(s: &str) -> Result<Address, Eip1193Error> {
    s.parse::<Address>().map_err(|e| Eip1193Error::Rpc(e.to_string()))
}

fn hex_qty(n: u64) -> String {
    format!("0x{n:x}")
}

fn parse_log(v: &Value) -> Result<RawLog, Eip1193Error> {
    let obj = v
        .as_object()
        .ok_or_else(|| Eip1193Error::Rpc("log entry is not an object".into()))?;

    let topics = obj
        .get("topics")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(b32_hex)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    let address = addr_hex(as_str(
        obj.get("address").ok_or_else(|| Eip1193Error::Rpc("log missing address".into()))?,
    )?)?;

    let data = obj.get("data").map(bytes_hex).transpose()?.unwrap_or_default();

    let block_number = obj.get("blockNumber").filter(|v| !v.is_null()).map(u64_hex).transpose()?;
    let block_timestamp = obj.get("blockTimestamp").filter(|v| !v.is_null()).map(u64_hex).transpose()?;
    let transaction_hash = obj
        .get("transactionHash")
        .and_then(Value::as_str)
        .map(b32_hex)
        .transpose()?;

    Ok(RawLog { topics, address, data, block_number, block_timestamp, transaction_hash })
}

#[async_trait]
impl<B: RpcBackend> Eip1193Provider for EthRpcEip1193<B> {
    async fn get_chain_id(&self) -> Result<u64, Eip1193Error> {
        u64_hex(&self.call("eth_chainId", json!([]))?)
    }

    async fn get_block_number(&self) -> Result<u64, Eip1193Error> {
        u64_hex(&self.call("eth_blockNumber", json!([]))?)
    }

    async fn logs(
        &self,
        address: Address,
        event_signature: Option<FixedBytes<32>>,
        from_block: Option<u64>,
        to_block: Option<u64>,
    ) -> Result<Vec<RawLog>, Eip1193Error> {
        let mut filter = json!({ "address": format!("{address:?}") });
        let m = filter.as_object_mut().unwrap();
        if let Some(sig) = event_signature {
            m.insert("topics".into(), json!([format!("{sig:?}")]));
        }
        if let Some(b) = from_block {
            m.insert("fromBlock".into(), json!(hex_qty(b)));
        }
        if let Some(b) = to_block {
            m.insert("toBlock".into(), json!(hex_qty(b)));
        }
        let res = self.call("eth_getLogs", json!([filter]))?;
        res.as_array()
            .ok_or_else(|| Eip1193Error::Rpc("eth_getLogs did not return an array".into()))?
            .iter()
            .map(parse_log)
            .collect()
    }

    async fn eth_call(&self, to: Address, data: Bytes) -> Result<Bytes, Eip1193Error> {
        let params = json!([
            { "to": format!("{to:?}"), "data": format!("0x{}", hex::encode(&data)) },
            "latest"
        ]);
        bytes_hex(&self.call("eth_call", params)?)
    }

    async fn estimate_gas(
        &self,
        to: Address,
        data: Bytes,
        from: Option<Address>,
    ) -> Result<u64, Eip1193Error> {
        let mut tx = json!({ "to": format!("{to:?}"), "data": format!("0x{}", hex::encode(&data)) });
        if let Some(f) = from {
            tx.as_object_mut().unwrap().insert("from".into(), json!(format!("{f:?}")));
        }
        u64_hex(&self.call("eth_estimateGas", json!([tx]))?)
    }

    async fn gas_price(&self) -> Result<u128, Eip1193Error> {
        u128_hex(&self.call("eth_gasPrice", json!([]))?)
    }

    async fn transaction_count(
        &self,
        address: Address,
        block: Option<u64>,
    ) -> Result<u64, Eip1193Error> {
        let blk = block.map(hex_qty).unwrap_or_else(|| "latest".into());
        u64_hex(&self.call("eth_getTransactionCount", json!([format!("{address:?}"), blk]))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Canned-response backend keyed by JSON-RPC method.
    #[derive(Default)]
    pub struct MockBackend {
        responses: Mutex<HashMap<String, Value>>,
    }
    impl MockBackend {
        fn with(method: &str, value: Value) -> Self {
            let m = Self::default();
            m.responses.lock().unwrap().insert(method.to_string(), value);
            m
        }
    }
    impl RpcBackend for MockBackend {
        fn rpc(&self, method: &str, _params: Value) -> Result<Value, String> {
            self.responses
                .lock()
                .unwrap()
                .get(method)
                .cloned()
                .ok_or_else(|| format!("no mock for {method}"))
        }
    }

    #[tokio::test]
    async fn chain_id_roundtrips_sepolia() {
        let backend = Arc::new(MockBackend::with("eth_chainId", json!("0xaa36a7"))); // 11155111
        let provider = EthRpcEip1193::new(backend);
        assert_eq!(provider.get_chain_id().await.unwrap(), 11155111);
    }

    #[tokio::test]
    async fn eth_call_decodes_hex_bytes() {
        let backend = Arc::new(MockBackend::with("eth_call", json!("0x1234")));
        let provider = EthRpcEip1193::new(backend);
        let out = provider
            .eth_call(Address::ZERO, Bytes::from(vec![0xab]))
            .await
            .unwrap();
        assert_eq!(out.to_vec(), vec![0x12, 0x34]);
    }

    #[tokio::test]
    async fn logs_parse_topics_and_data() {
        let log = json!({
            "address": "0x000000000000000000000000000000000000dead",
            "topics": ["0x".to_string() + &"11".repeat(32)],
            "data": "0xc0ffee",
            "blockNumber": "0x10",
            "transactionHash": "0x".to_string() + &"22".repeat(32),
        });
        let backend = Arc::new(MockBackend::with("eth_getLogs", json!([log])));
        let provider = EthRpcEip1193::new(backend);
        let logs = provider.logs(Address::ZERO, None, Some(1), Some(2)).await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].topics.len(), 1);
        assert_eq!(logs[0].data.to_vec(), vec![0xc0, 0xff, 0xee]);
        assert_eq!(logs[0].block_number, Some(16));
    }
}
