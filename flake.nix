{
  description = "rhizome";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs =
    { self
    , nixpkgs
    , flake-utils
    , rust-overlay
    } @ inputs:
    flake-utils.lib.eachDefaultSystem (system:
    let
      overlays = [ (import rust-overlay) ];
      pkgs = import nixpkgs { inherit system overlays; };

      rust-toolchain =
        (pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml).override {
          extensions = [ "cargo" "clippy" "rustfmt" "rust-src" "rust-std" ];
          targets = [ "wasm32-unknown-unknown" ];
        };

      nightly-rustfmt = pkgs.rust-bin.nightly.latest.rustfmt;

      format-pkgs = with pkgs; [
        nixpkgs-fmt
      ];

      cargo-installs = with pkgs; [
        binaryen
        cargo-deny
        cargo-expand
        cargo-sort
        cargo-udeps
        cargo-watch
        wasm-bindgen-cli
      ];
    in
    rec
    {
      devShells.default = pkgs.mkShell {
        name = "rhizome";
        nativeBuildInputs = with pkgs; [
          # The ordering of these two items is important. For nightly rustfmt to be used instead of
          # the rustfmt provided by `rust-toolchain`, it must appear first in the list. This is
          # because native build inputs are added to $PATH in the order they're listed here.
          nightly-rustfmt
          rust-toolchain
          pre-commit
          direnv
          self.packages.${system}.irust
          binaryen
          nodejs
        ] ++ format-pkgs ++ cargo-installs;

      shellHook = ''
        [ -e .git/hooks/pre-commit ] || pre-commit install --install-hooks && pre-commit install --hook-type commit-msg
      '';
      };

      packages.irust = pkgs.rustPlatform.buildRustPackage rec {
        pname = "irust";
        version = "1.65.1";
        src = pkgs.fetchFromGitHub {
          owner = "sigmaSd";
          repo = "IRust";
          rev = "v${version}";
          sha256 = "sha256-AMOND5q1XzNhN5smVJp+2sGl/OqbxkGPGuPBCE48Hik=";
        };

        doCheck = false;
        cargoSha256 = "sha256-A24O3p85mCRVZfDyyjQcQosj/4COGNnqiQK2a7nCP6I=";
      };
    }
  );
}
