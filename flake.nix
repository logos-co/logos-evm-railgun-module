{
  description = "Logos RAILGUN module — private transactions (shield / private transfer / unshield) for the EVM wallet. Wraps the native railgun-rs engine. Sepolia-first; UNAUDITED upstream.";

  inputs = {
    # module-builder master now bundles the logos-rust-sdk single-mode `Send`-lift
    # (mb#138) that the RAILGUN engine's non-`Send` signer needs, so a standalone
    # `#lgx` build (e.g. a downstream doctest dependency) picks it up directly —
    # no explicit logos-rust-sdk threading required.
    logos-module-builder.url = "github:logos-co/logos-module-builder";

    # Dependency modules — their published `.lidl`s drive the generated typed
    # `modules().<dep>` clients. `eth_rpc_module` backs the engine's Eip1193
    # provider (all chain reads) + the proxied bundler submit (`raw_rpc_url`);
    # `keystore_module` signs the relayer's userOp/7702 digests (`sign_digest`,
    # EOA key stays in keystore). `follows` keeps the same module-builder.
    #
    # TEMPORARY: eth_rpc_module is pinned to the feature commit that carries
    # `raw_rpc_url` (eth-rpc#4, still open); revert to a plain URL once it merges.
    # keystore's `sign_digest` landed on main (keystore#4) — plain URL again.
    eth_rpc_module = {
      url = "github:logos-co/logos-evm-eth-rpc-module/a4b2b284409f796ab35961aeafbd91cc81dadc4c";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    keystore_module = {
      url = "github:logos-co/logos-evm-keystore-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;
    in
    {
      packages = forAllSystems (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
