//! RAILGUN key handling — deriving the public `0zk` address and building the
//! engine signer from raw spending/viewing keys.
//!
//! ## Security boundary
//! The RAILGUN **spending key** is a Groth16 witness: it must be present in this
//! process during proof generation. So — exactly like `keystore_module` owns the
//! EOA private keys — the railgun spending/viewing keys live **inside this
//! module** and are never returned over the IPC JSON boundary. Only the public
//! `0zk` address, balances, and proofs cross the bridge.
//!
//! Keys are raw 32-byte scalars (BabyJubJub spending, X25519/ed25519 viewing).
//! Single-seed derivation from the keystore mnemonic is a follow-up; for now keys
//! are imported directly (hex) and held by the module's key store.

use std::sync::Arc;

use alloy::primitives::keccak256;
use railgun::account::chain::ChainId;
use railgun::account::signer::{PrivateKeySigner, RailgunSigner};
use railgun::crypto::keys::{HexKey, SpendingKey, ViewingKey};

fn parse_keys(spending_hex: &str, viewing_hex: &str) -> Result<(SpendingKey, ViewingKey), String> {
    let sk = SpendingKey::from_hex(spending_hex.trim_start_matches("0x"))
        .map_err(|e| format!("invalid spending key: {e}"))?;
    let vk = ViewingKey::from_hex(viewing_hex.trim_start_matches("0x"))
        .map_err(|e| format!("invalid viewing key: {e}"))?;
    Ok((sk, vk))
}

/// Build an engine signer (held in-module) from raw hex keys, bound to `chain`.
pub fn make_signer(
    spending_hex: &str,
    viewing_hex: &str,
    chain: ChainId,
) -> Result<Arc<PrivateKeySigner>, String> {
    let (sk, vk) = parse_keys(spending_hex, viewing_hex)?;
    Ok(PrivateKeySigner::new(sk, vk, chain))
}

/// Derive the public `0zk1…` RAILGUN address from raw hex keys, bound to `chain`.
pub fn derive_zk_address(
    spending_hex: &str,
    viewing_hex: &str,
    chain: ChainId,
) -> Result<String, String> {
    Ok(make_signer(spending_hex, viewing_hex, chain)?.address().to_string())
}

/// Deterministically derive railgun spending + viewing keys (as hex) from an
/// opaque `seed` — typically a *deterministic* EOA signature obtained from
/// `keystore_module.sign_message` over a fixed message. keccak256 domain
/// separation makes the two keys independent; railgun reduces each 32-byte value
/// into its curve scalar on use, so any seed yields a valid key pair.
///
/// This binds the railgun wallet to the EOA: the same EOA always reproduces the
/// same seed → same `0zk` address, so funds are recoverable as long as the user
/// controls the EOA. ⚠️ It is NOT the RAILGUN-Community canonical BIP-32
/// derivation, so funds are not recoverable in a third-party RAILGUN wallet;
/// canonical-recovery derivation is a follow-up.
pub fn derive_keys_from_seed(seed: &[u8]) -> (String, String) {
    let spend = keccak256([seed, b"logos-railgun/spend/v1"].concat());
    let view = keccak256([seed, b"logos-railgun/view/v1"].concat());
    (hex::encode(spend), hex::encode(view))
}

/// Convenience: the chain binding for an EVM chain id, or the chain-agnostic
/// `All` binding (the universal address shared across EVM chains).
pub fn chain_binding(chain_id: Option<u64>) -> ChainId {
    match chain_id {
        Some(id) => ChainId::evm(id),
        None => ChainId::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reuses the engine's own `account::signer` test vector to guarantee our
    // derivation matches railgun-rs exactly.
    const SPENDING: &str = "039b3b11110e49d7340cbe7171791972e3c0d94ef31b18d6ab93d7ace62d278a";
    const VIEWING: &str = "d345b2cc2f414aa93413b9572fa2b26e0e869e9274b006415a8d62ab1fa2dcb1";
    const EXPECTED: &str = "0zk1qynw6pq3nvntq90sts0khgs8ndqxzsrza88cd553dqwt28mskxlxtrv7j6fe3z53l7lczqdhfmfffxa8cps4hw7nprhx3hv3ykx097l8p7gjh2xla365qacrwu2";

    #[test]
    fn derives_known_address() {
        assert_eq!(derive_zk_address(SPENDING, VIEWING, ChainId::All).unwrap(), EXPECTED);
    }

    #[test]
    fn accepts_0x_prefix() {
        let with_prefix = derive_zk_address(
            &format!("0x{SPENDING}"),
            &format!("0x{VIEWING}"),
            ChainId::All,
        )
        .unwrap();
        assert_eq!(with_prefix, EXPECTED);
    }

    #[test]
    fn rejects_bad_key() {
        assert!(derive_zk_address("notahexkey", VIEWING, ChainId::All).is_err());
    }

    #[test]
    fn seed_derivation_is_deterministic_and_valid() {
        // A fixed "EOA signature" seed → a stable, valid 0zk address.
        let seed = b"deterministic eoa signature bytes (fixed for the test)";
        let (sk, vk) = derive_keys_from_seed(seed);
        let addr1 = derive_zk_address(&sk, &vk, ChainId::All).expect("derived keys must be valid");
        assert!(addr1.starts_with("0zk1"), "got {addr1}");
        // Same seed → same address (recoverable); different seed → different address.
        let (sk2, vk2) = derive_keys_from_seed(seed);
        assert_eq!((&sk, &vk), (&sk2, &vk2));
        let (sk3, _) = derive_keys_from_seed(b"a different seed");
        assert_ne!(sk, sk3);
    }
}
