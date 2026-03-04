{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    yoloproj = {
      url = "github:yolodev/yoloproj";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ yoloproj, ... }:
    yoloproj.lib.mkFlake inputs {
      # debug = true;

      imports = [
        yoloproj.flakeModules.rust
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
        {
          project.rust.toolchainFile = ./rust-toolchain.toml;

          devshells.default.packages = [
            pkgs.espflash
            pkgs.esp-generate

            pkgs.ldproxy
            pkgs.libusb1
            pkgs.clang
          ];

          pre-commit.check.enable = false; # Cannot run the crate2nix hook without network access
          pre-commit.settings.hooks = {
            # clippy.enable = false;
          };
        };
    };
}
