# Running the RAILGUN Module Against logoscore

`logos-evm-railgun-module` adds **private transactions** to the Logos
multi-chain EVM wallet. It wraps the native **railgun-rs** shielded-pool engine
(`ethereum/kohaku`): *shield* public ERC-20 into a private pool, *transfer*
privately (`0zk → 0zk`, amounts and parties hidden by Groth16 zk-proofs), and
*unshield* back to a public `0x` address — with an ERC-4337 relayer path that
also hides the sender.

> ⚠️ **The upstream engine is unaudited and not production-ready.** This module
> is **Sepolia-first**; mainnet is gated. Do not move real funds with it.

The railgun spending/viewing keys are a Groth16 witness, so they live **inside
this module** and never cross the IPC boundary — mirroring how `keystore_module`
holds the EOA key. Chain reads go through `eth_rpc_module` and relayer submits
are proxied; both are declared dependencies.

This doc-test exercises the module end-to-end through the headless `logoscore`
runtime. Everything here is **offline and deterministic** — it builds and
registers the engine, derives the public `0zk` address, and builds a shield
transaction, none of which touch the network (proving, sync, and the relayer
need a live chain + bundler and are out of scope for an offline test):

1. Build `logoscore` and `lgpm` from their published flakes.
2. Build this module's `.lgx` **and its `eth_rpc_module` + `keystore_module`
   dependencies'** `.lgx`, then install all three.
3. Start a `logoscore` daemon, load `railgun_module` (its deps auto-resolve),
   and drive it: introspect the API, build the engine on Sepolia from known
   keys, read back the `0zk` address, and prepare a shield transaction.

**What you'll build:** This `railgun_module` (plus its `eth_rpc`/`keystore` deps), packaged as `.lgx`, installed with `lgpm`, and called through a `logoscore` daemon.

**What you'll learn:**

- How a Rust cdylib Logos module that wraps a native zk engine is packaged as an installable `.lgx`
- How to install a module together with its dependency modules and load it into a `logoscore` daemon
- How RAILGUN builds a shielded-pool engine and derives a public `0zk` address offline
- How a shield (public → private deposit) is prepared as unsigned `TxData` for the caller to sign
- How the railgun keys stay inside the module (only the public `0zk` address and unsigned txs come out)

## Prerequisites

- **Nix** with flakes enabled. Install from [nixos.org](https://nixos.org/download.html), then enable flakes:

```bash
mkdir -p ~/.config/nix
echo 'experimental-features = nix-command flakes' >> ~/.config/nix/nix.conf
```

Verify: `nix flake --help >/dev/null 2>&1 && echo "Flakes enabled"`

- **A Linux or macOS machine.** Every step here is offline.

---

## Step 1: Build logoscore and lgpm

`logoscore` is the headless frontend for `logos-liblogos` (it brings in the
whole module-runtime stack), and `lgpm` installs `.lgx` packages into a
modules directory.

### 1.1 Build logoscore

```bash
nix build 'github:logos-co/logos-logoscore-cli#cli' --out-link ./logos
```

### 1.2 Build lgpm

```bash
nix build 'github:logos-co/logos-package-manager#cli' -o lgpm
```

---

## Step 2: Build and install railgun + its dependencies

`railgun_module` depends on `eth_rpc_module` (chain reads) and
`keystore_module` (the relayer's EOA signing). The daemon must have all
three installed before it can load railgun, so build each `.lgx` and install
them into a local `./modules` directory. The bundled `capability_module`
(shipped with `logoscore`) handles the load-time auth handshake, so seed it
first.

> The first build compiles the native railgun engine (zk-proving deps), so
> allow a generous timeout.

### 2.1 Build the railgun module's .lgx

```bash
# From inside the clone this is simply: nix build '.#lgx'
nix build 'github:logos-co/logos-evm-railgun-module#lgx' -o railgun-lgx
```

```bash
ls railgun-lgx/*.lgx
```

### 2.2 Build the eth_rpc_module dependency .lgx

```bash
nix build 'github:logos-co/logos-evm-eth-rpc-module#lgx' --no-write-lock-file -o eth-rpc-lgx
```

```bash
ls eth-rpc-lgx/*.lgx
```

### 2.3 Build the keystore_module dependency .lgx

```bash
nix build 'github:logos-co/logos-evm-keystore-module#lgx' --no-write-lock-file -o keystore-lgx
```

```bash
ls keystore-lgx/*.lgx
```

### 2.4 Seed the capability module

```bash
mkdir -p modules
cp -RL ./logos/modules/. ./modules/

```

### 2.5 Install all three .lgx with lgpm

```bash
./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file eth-rpc-lgx/*.lgx
./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file keystore-lgx/*.lgx
./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file railgun-lgx/*.lgx

```

### 2.6 Confirm the install

```bash
./lgpm/bin/lgpm --modules-dir ./modules list
```

---

## Step 3: Run the daemon and drive the RAILGUN engine

Start `logoscore` pointed at `./modules` and load `railgun_module` — its
`eth_rpc_module` and `keystore_module` dependencies auto-resolve. Then build
the engine on **Sepolia** (chain id `11155111`) from a known spending/viewing
key pair and read back the public `0zk` address.

### 3.1 Write the engine init config

`init` takes `{ chainId, spendingKey, viewingKey, poi }`. The keys are
railgun (not EOA) keys; they are imported into the module and never
returned. We use a fixed test pair so the derived `0zk` address is
deterministic.

```json
{
  "chainId": 11155111,
  "spendingKey": "039b3b11110e49d7340cbe7171791972e3c0d94ef31b18d6ab93d7ace62d278a",
  "viewingKey": "d345b2cc2f414aa93413b9572fa2b26e0e869e9274b006415a8d62ab1fa2dcb1",
  "poi": false
}
```

### 3.2 Write the shield request

A shield deposits a public ERC-20 into the shielded pool. `prepare_shield`
takes `{ asset, amount }` (amount is a decimal string of base units) and
returns the unsigned `TxData[]` for the caller to approve + sign + send.

```json
{
  "asset": "0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238",
  "amount": "1000000"
}
```

### 3.3 Start the daemon

```bash
logoscore -D -m ./modules > logs.txt &
```

```bash
sleep 3
```

### 3.4 Load the module (deps auto-resolve)

```bash
logoscore load-module railgun_module
```

### 3.5 Introspect the module

```bash
logoscore module-info railgun_module
```

### 3.6 Build the engine on Sepolia (offline)

`init` builds and registers the railgun engine for Sepolia and returns
the public `0zk` address. This performs **no network I/O** — the engine
is constructed and the keys registered entirely in-process.

```bash
logoscore call railgun_module init @init.json
```

### 3.7 Read the public 0zk address

```bash
logoscore call railgun_module get_zk_address
```

### 3.8 Prepare a shield transaction

`prepare_shield` builds the unsigned deposit calldata — pure `TxData`
(`to`/`data`/`value`), no zk-proof and no network. The wallet backend
would prepend an ERC-20 `approve`, sign each with `keystore_module`, and
broadcast through `eth_rpc_module`.

```bash
logoscore call railgun_module prepare_shield @shield.json
```

### 3.9 Stop the daemon

```bash
logoscore stop
```

```bash
sleep 2
```

### 3.10 Confirm the daemon has stopped

```bash
logoscore status
```
