# `logos-evm-railgun-module` — Reference Specification

> ⚠️ **UNAUDITED. Sepolia-first.** This module wraps the **native `railgun-rs`
> engine** (from [`ethereum/kohaku`](https://github.com/ethereum/kohaku)), which is
> explicitly **not audited / not production-ready**, and it moves user funds
> privately. The whole feature ships **Sepolia-default with prominent warnings**;
> mainnet is gated behind an explicit chain selection. Do not move mainnet funds.

## Purpose

Adds **private transactions** to the Logos EVM wallet via **RAILGUN** — a
shielded-pool privacy system:

- **shield** — deposit a public ERC-20 into the private pool (`0x…` → `0zk…`),
- **private transfer** — move funds `0zk… → 0zk…`, amounts and parties hidden by
  Groth16 zk-proofs,
- **unshield** — withdraw from the pool back to a public address (`0zk… → 0x…`),
- **relayed send** — the same private transfer / unshield, but broadcast through an
  **ERC-4337 bundler** so the *sender* is hidden too (no EOA in the public tx).

The module is a Rust **cdylib** Logos module (same pattern as
`keystore`/`eth-rpc`/`uniswap`). It exposes 9 `Q_INVOKABLE`-equivalent methods; all
structured values cross the IPC boundary as JSON strings (`{ "ok": true, … }` /
`{ "ok": false, "error": "…" }`).

## Overall architecture

```
wallet_backend_module  (coordinator: init_private / shield / private_send / view)
   │   modules().railgun_module.*
   ▼
railgun_module  (THIS — Rust cdylib, concurrency:single)
   ├─ RailgunEngine  → one long-lived railgun-rs RailgunProvider per chain
   ├─ adapter A: Eip1193Provider  ── chain reads ─→ modules().eth_rpc_module.raw_rpc
   ├─ adapter B: Database (DiskDatabase) ── note/merkle state under the instance dir
   ├─ key store: spending/viewing keys derived in-module, NEVER returned over IPC
   ├─ keystore-bridge Signer ── userOp/7702 signing ─→ modules().keystore_module.sign_digest
   └─ 4337 submit ── eth_sendUserOperation ─→ modules().eth_rpc_module.raw_rpc_url (proxied)
```

- **Adapter A — `Eip1193Provider` over `eth_rpc_module`** (`src/rpc_backend.rs`):
  implements the engine's async EIP-1193 trait (chainId, blockNumber, getLogs,
  eth_call, estimateGas, gasPrice, getTransactionCount) by forwarding raw JSON-RPC
  to `eth_rpc_module.raw_rpc(chainId, method, params)`. This is the single bridge
  from RAILGUN to chain data, so it inherits eth-rpc's fail-closed proxy.
- **Adapter B — `Database`** (`src/db_adapter.rs`): a disk-backed `Database`
  (async get/set/delete over `&[u8]`) under
  `RustModuleContext.instance_persistence_path/chain-<id>/`. Engine UTXO keys can
  exceed OS filename limits, so the on-disk filename is `hex(keccak256(key))`;
  writes are atomic (temp + rename). Persisted note data is sensitive — it stays
  under the per-instance dir.
- **Engine lifecycle** (`src/engine.rs`): `RailgunBuilder::new(chainConfig, adapterA)
  .with_database(adapterB).build()` once, then `register(signer)`. Held behind
  `&mut self` (concurrency:single).
- **Keys** (`src/keys.rs`): the railgun **spending key is a Groth16 witness** — it
  must be present in-process during proving. So, exactly like `keystore_module`
  owns the EOA keys, the railgun spending/viewing keys live **inside this module**
  and never cross IPC. Only the public `0zk` address, balances, proofs and unsigned
  txs leave the module.

## Communication with dependencies

`dependencies` (metadata.json): `["eth_rpc_module", "keystore_module"]`.

| Call | Used for |
|---|---|
| `eth_rpc_module.raw_rpc(chainId, method, params)` | every engine chain read (via adapter A) |
| `eth_rpc_module.raw_rpc_url(chainId, url, method, params)` | submit `eth_sendUserOperation` to the bundler **through net-proxy** |
| `keystore_module.sign_digest(owner, digestHex)` | sign the relayer's userOp hash + its EIP-7702 authorization (EOA key stays in keystore) |

The 4337 **submit** is routed through `eth_rpc` (not a module-owned HTTP client) so
that a private send goes through the same fail-closed proxy as everything else — a
private send must not leak the user's IP to the bundler.

## Full API reference

All amounts are **decimal strings** in base units (u128 wei exceeds JSON's safe
integer range). Addresses accept `0x`-prefixed or bare hex.

### `init(params_json) → { ok, address }`
One-time load with **explicit** keys. `params`:
`{ "chainId": u64, "spendingKey": hex, "viewingKey": hex, "poi": bool }`.
Builds + registers the engine for the chain (offline — no network) and returns the
public `0zk1…` address. Supported chains: mainnet (1) + Sepolia (11155111).

### `init_from_seed(params_json) → { ok, address }`
Like `init` but derives the railgun keys from an opaque `seed`:
`{ "chainId": u64, "seed": hex, "poi": bool }`. The backend passes a **deterministic
EOA signature** (`keystore.sign_message` over a fixed message) as the seed; the
spending/viewing keys are derived in-module (keccak domain separation) and never
returned. Binds the railgun wallet to the EOA (same EOA → same `0zk` address).
> Not yet the RAILGUN-Community canonical BIP-32 derivation, so funds are only
> recoverable in a wallet that can reproduce the same EOA signature → seed.

### `get_zk_address() → { ok, address }`
The public `0zk1…` address (requires `init`/`init_from_seed`).

### `sync() → { ok }`
Sync UTXO/TXID (and POI, if enabled) state to the latest block. Needs a live chain.

### `get_shielded_balance() → { ok, balances: [BalanceEntry] }`
Per-asset shielded balance. Each entry is `{ asset: { erc20 }, amount, poiStatus }`.

### `prepare_shield(params_json) → { ok, txs: [TxData] }`
SHIELD (public → private). `params`: `{ "asset": "0x…", "amount": "decimal" }`.
Returns the **unsigned** `TxData[]` (`{ to, data, value }`) — pure calldata, **no
proof, no network**. The caller (backend) first `approve`s the RAILGUN smart wallet
for the ERC-20, then signs + broadcasts each tx (keystore + eth_rpc). The shield
tx's `to` is the RAILGUN smart wallet (the approve spender).

### `prepare_transfer(params_json) → { ok, tx: TxData }`
Private TRANSFER (`0zk → 0zk`). `params`:
`{ "to": "0zk…", "asset", "amount", "memo"? }`. Runs **Groth16 proving** (needs the
spending key + circuit artifacts) and returns the proven `TxData` (a call to the
RAILGUN smart wallet) for **self-broadcast** (sender EOA visible; amounts/parties
hidden). No fee (internal transfer).

### `prepare_unshield(params_json) → { ok, tx: TxData }`
UNSHIELD (`private → 0x`). `params`: `{ "to": "0x…", "asset", "amount" }`. Groth16
proving; returns the proven `TxData`. The engine adds the chain's unshield fee so
the recipient receives the exact amount.

### `relayed_send(params_json) → { ok, userOpHash }`
RELAYED private send — the **ERC-4337 broadcaster** path that **hides the sender**.
`params`: `{ "to": "0zk…"|"0x…", "asset", "amount", "memo"?, "owner": "0x…",
"bundlerUrl": "https://…" }`. Routes `0zk` → transfer, `0x` → unshield, wraps the
RAILGUN tx in a **7702 UserOperation** paid for out of the shielded pool (the
in-module railgun signer authorizes a fee note to the privacy paymaster), **signs**
the userOp (and its 7702 authorization) via `keystore.sign_digest` (EOA key stays
in keystore), and **submits** to `bundlerUrl` via `eth_rpc.raw_rpc_url` (proxied).
Needs a live bundler + chain (the fee estimate iterates against both) — there is no
offline path. The fee token is fixed to the chain's wrapped base token.

## Security model & invariants

1. **Railgun keys never leave the module.** The spending key is a proving witness;
   spending/viewing keys live in-process and are never returned over IPC. Only
   public artifacts (the `0zk` address, balances, proofs, unsigned txs) cross.
2. **The EOA key never leaves keystore.** The relayer signs via a keystore-bridge
   `Signer` whose `sign_hash` calls `keystore.sign_digest` over IPC. The userOp's
   EIP-712 signing hash is private to the engine, so signing happens *in-module*
   with the bridge — the key is never relayed.
3. **All network goes through `eth_rpc` → net-proxy.** Chain reads (adapter A) and
   the bundler submit (`raw_rpc_url`) are fail-closed proxied. A private send must
   not degrade to leaking the user's IP. (Circuit-artifact downloads during proving
   are a known exception — see below.)
4. **Sepolia-first, mainnet-gated, unaudited.** The engine is unaudited; the UI and
   this spec carry the warning; the default chain is Sepolia.
5. **EOA-bound key derivation.** `init_from_seed` derives the railgun wallet from a
   deterministic EOA signature, so there is no separate seed to back up; recovery
   follows EOA control.

### Known limitations / follow-ups
- **Circuit artifacts** are downloaded at proving time by the engine's default
  `RemoteArtifactLoader` (a third-party GitHub repo) — un-proxied and not pinned.
  `RemoteArtifactLoader::new(base_url)` is public, so pinning/bundling a controlled
  artifact source is an upstream-contributable `with_artifact_loader` hook, not a
  fork. Until then, proving (`prepare_transfer`/`prepare_unshield`/`relayed_send`)
  needs network reachability to that source.
- **Canonical recovery**: `init_from_seed` is not yet RAILGUN-Community BIP-32.
- **UserOp status**: `relayed_send` returns the `userOpHash`; polling its receipt
  (`eth_getUserOperationReceipt`) is coordinator/UI follow-up work.

## Build, run & test

```bash
# Pure core only — no Logos runtime / generated scaffold needed.
( cd rust-lib && cargo test --no-default-features )

# Full module (Qt plugin) via nix. The engine's alloy/ruint need rustc ≥1.91, so
# metadata.json sets nix.rust.toolchain = "1.96.0" (rust-overlay in the builder).
nix build .#default \
  --override-input logos-module-builder        path:<ws>/repos/logos-module-builder \
  --override-input logos-module-builder/logos-rust-sdk path:<ws>/repos/logos-rust-sdk \
  --override-input eth_rpc_module               path:<ws>/repos/eth-rpc-module \
  --override-input keystore_module              path:<ws>/repos/keystore-module

lm methods ./result/lib/railgun_module_plugin.dylib   # 9 invokables
```

### Offline doc-test (end-to-end against `logoscore`)

`doctests/railgun-module-runtime.test.yaml` is an executable doc-test (run via the
shared [`logos-doctest`](https://github.com/logos-co/logos-doctest) CLI). It builds
this module's `.lgx` **and its `eth_rpc`/`keystore` dependency `.lgx`**, installs all
three with `lgpm`, loads `railgun_module` in a `logoscore` daemon (deps auto-resolve),
and drives the **offline** surface — `init` (build + register the Sepolia engine),
`get_zk_address`, and `prepare_shield` (unsigned `TxData`) — none of which touch the
network. Proving, `sync`, and the relayer need a live chain + bundler and are out of
scope for the offline test.

```bash
( cd doctests && ./run.sh )   # runs every *.test.yaml + regenerates outputs/*.md
# or a single spec:
nix run github:logos-co/logos-doctest -- run doctests/railgun-module-runtime.test.yaml --verbose
```

`metadata.json` highlights: `interface: cdylib`, `concurrency: single`,
`dependencies: [eth_rpc_module, keystore_module]`, `nix.rust.toolchain: "1.96.0"`,
`nix.rust.packages.build: [cmake, pkg-config, rustPlatform.bindgenHook]`, and the
`[patch.crates-io]` block (`ruint`, `ark-circom`) the engine's git deps require.

## Concurrency

`concurrency: "single"`. The engine (`RailgunProvider`) is a single `&mut`-driven
object whose `dyn RailgunSigner` field is not `Send + Sync`, so it is held directly
behind `&mut self`. A long proof blocks the module's dispatch thread. (`single`
also lets the SDK lift the `Send` bound on the module instance — the module runs on
its subprocess's single event-loop thread.) Each async engine op is driven on a
per-call current-thread tokio runtime so the engine's outbound `modules()` IPC has
the dispatch thread's event loop.
