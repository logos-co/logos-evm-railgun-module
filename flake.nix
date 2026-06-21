{
  description = "Logos RAILGUN module — private transactions (shield / private transfer / unshield) for the EVM wallet. Wraps the native railgun-rs engine. Sepolia-first; UNAUDITED upstream.";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";

    # Dependency module — its published `.lidl` drives the generated
    # `modules().eth_rpc_module` client used for all chain reads (the engine's
    # Eip1193 provider is backed by it). `follows` keeps the same module-builder.
    eth_rpc_module = {
      url = "github:logos-co/logos-evm-eth-rpc-module";
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
