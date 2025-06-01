{
  description = "Flake for creusot";

  inputs = {
    nixpks.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    nixpkgs,
    rust-overlay,
    flake-utils,
    naersk,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        overlays = [(import rust-overlay)];
        pkgs = import nixpkgs {
          inherit system overlays;
          config.allowUnfree = true;
        };
        rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain;
        naerskLib = pkgs.callPackage naersk {
          cargo = rust;
          rustc = rust;
        };
        build_inputs = with pkgs; [
          rust
          pkg-config
          openssl
          opam
          gcc
          autoconf
          gtk3
          gtksourceview
          ocamlPackages.zmq
          cairo
          zeromq
          why3
          z3
        ];
        tooling = with pkgs; [
          deadnix # Dead code detection for nix
          statix # Highlights nix antipatterns
          taplo # Toml toolkit with formatter
        ];
      in
        with pkgs; {
          devShells.default = mkShell {
            buildInputs = build_inputs ++ tooling;
            RUST_BACKTRACE = "full";
            LD_LIBRARY_PATH = "${lib.makeLibraryPath (build_inputs)}";
          };
        }
    );
}
