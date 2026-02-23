{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    rust-overlay,
    microvm,
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };
        rustToolchain = pkgs.rust-bin.stable.latest.default;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
        vitroSource = let
          nixFilter = path: _type: builtins.match ".*nix/lib/disk-.*\\.nix$" path != null;
          filter = path: type:
            (nixFilter path type) || (craneLib.filterCargoSources path type);
        in pkgs.lib.cleanSourceWith {
          src = ./.;
          inherit filter;
        };
        commonArgs = {
          pname = "vitro";
          version = "0.1.0";
          src = vitroSource;
          buildInputs = [pkgs.openssl];
          nativeBuildInputs = [pkgs.pkg-config];
        };
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        vitro = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          cargoExtraArgs = "-p vitro";
          nativeBuildInputs = (commonArgs.nativeBuildInputs or []) ++ [pkgs.makeWrapper];
          postInstall = ''
            wrapProgram $out/bin/vitro \
              --prefix PATH : ${pkgs.lib.makeBinPath [pkgs.age pkgs.openssh pkgs.autossh]}
            wrapProgram $out/bin/git-remote-vitro \
              --prefix PATH : ${pkgs.lib.makeBinPath [pkgs.age pkgs.openssh]}
          '';
        });
      in {
        formatter = pkgs.alejandra;

        packages.default = vitro;

        devShells.default = pkgs.mkShell {
          buildInputs = [
            vitro
            rustToolchain
            pkgs.rust-analyzer
            pkgs.pkg-config
            pkgs.openssl
            pkgs.uv
          ];
        };

        checks = {
          proxy = import ./nix/tests/proxy.nix {
            inherit nixpkgs system;
            vitroServerModule = self.nixosModules.server;
          };
        };
      }
    )
    // {
      lib.mkHost = import ./nix/lib/mkHost.nix;
      lib.mkEnv = import ./nix/lib/mkEnv.nix;

      nixosModules.server = {
        config,
        lib,
        pkgs,
        ...
      }: {
        imports = [
          microvm.nixosModules.host
          ./nix/modules/host.nix
        ];

        nixpkgs.overlays = [
          (final: prev: {
            vitro = self.packages.${final.stdenv.hostPlatform.system}.default;
          })
        ];

        _module.args.inputs = {
          inherit microvm;
        };
      };

      nixosModules.client = {...}: {
        imports = [./nix/modules/client.nix];
        nixpkgs.overlays = [
          (final: prev: {
            vitro = self.packages.${final.stdenv.hostPlatform.system}.default;
          })
        ];
      };
    };
}
