{
  description = "Lean dev-shell for supabase-realtime-rs (build + tests)";

  inputs = {
    nixpkgs      .url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay .url = "github:oxalica/rust-overlay";
    flake-utils  .url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs     = import nixpkgs { inherit system overlays; };

        # — Rust nightly toolchain with source + rust-analyzer ―
        rustToolchain =
          pkgs.rust-bin.selectLatestNightlyWith (toolchain:
            toolchain.default.override {
              extensions = [ "rust-src" "rust-analyzer" ];
                    /* NEW — ship std + core for WASM */
                    targets    = [ "wasm32-unknown-unknown" ];
            });

        # — native libraries reqwest/openssl need at build time ―
        nativeLibs = with pkgs;
          [ openssl pkg-config cargo-make wasm-pack ]
          ++ lib.optionals stdenv.isDarwin [ darwin.apple_sdk.frameworks.Security ];
      in

      # `rec` makes the attribute set self-referential so we can
      # say `stable = default` without the error you hit.
      {
        devShells = rec {
          default = pkgs.mkShell {
            buildInputs = [ rustToolchain ] ++ nativeLibs;

            shellHook = ''
              echo "🦀  supabase-realtime-rs dev-shell ready (nightly + OpenSSL)"
            '';
          };

          stable  = default;
          nightly = default;
        };
      });
}
