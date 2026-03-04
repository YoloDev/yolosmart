{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    yoloproj = {
      url = "github:yolodev/yoloproj";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # Binary cache from eigenvalue, so we do not follow nixpkgs
    crate2nix.url = "github:nix-community/crate2nix";
  };

  nixConfig = {
    extra-trusted-public-keys = "eigenvalue.cachix.org-1:ykerQDDa55PGxU25CETy9wF6uVDpadGGXYrFNJA3TUs=";
    extra-substituters = "https://eigenvalue.cachix.org";
    allow-import-from-derivation = true;
  };

  outputs =
    inputs@{ yoloproj, ... }:
    yoloproj.lib.mkFlake inputs {
      # debug = true;

      imports = [
        yoloproj.flakeModules.oci
      ];

      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      # rust-overlay.toolchainFile = ./rust-toolchain.toml;

      perSystem =
        {
          pkgs,
          # lib,
          # config,
          ...
        }:
        let
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          # rust = pkgs.rust-bin.stable.latest.default.override {
          #   extensions = [ "rust-src" ];
          #   targets = [
          #     "riscv32imc-unknown-none-elf"
          #     "riscv32imac-unknown-none-elf"
          #   ];
          # };
        in
        {
          pkgs.overlays = [ (import inputs.rust-overlay) ];

          packages = {
            inherit rust;
          };

          devshells.default.packages = [
            rust

            pkgs.jq

            pkgs.espflash
            pkgs.esp-generate

            pkgs.ldproxy
            pkgs.libusb1
            pkgs.clang

            # zed language servers
            pkgs.crates-lsp
            pkgs.package-version-server
          ];

          pre-commit.check.enable = false; # Cannot run the crate2nix hook without network access
          pre-commit.settings.hooks = {
            rustfmt.enable = true;

            # clippy = {
            #   enable = true;
            #   settings.denyWarnings = true;
            #   settings.extraArgs = "--all";
            #   settings.offline = false;

            #   package = rust;
            #   packageOverrides.cargo = rust;
            #   packageOverrides.clippy = rust;
            # };

            rustfmt = {
              package = rust;
              packageOverrides.cargo = rust;
              packageOverrides.rustfmt = rust;
            };
          };
        };
    };
}
