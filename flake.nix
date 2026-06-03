{
  description = "orbit-etl — multi-domain Rust platform (ETL + LLM agent + satellite/geo)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Pinned dev toolchain (intentionally newer than the 1.88 MSRV in
        # Cargo.toml; this is the dev shell, not the MSRV floor).
        rust-toolchain = pkgs.rust-bin.stable."1.95.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
        };

        # System libs needed by orbit-geo's GDAL + STAC + async-tiff stack.
        nativeBuildInputs = with pkgs; [
          rust-toolchain
          pkg-config
          cmake
          protobuf       # for orbit-proto (tonic-prost-build)
        ];

        buildInputs = with pkgs; [
          gdal
          openssl
          libiconv
          sqlite
        ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
          # macOS frameworks needed by gdal/rustls.
          pkgs.darwin.apple_sdk.frameworks.Security
          pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
        ];

      in {
        # `nix develop` → drops into a shell with the full toolchain.
        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;

          # Help `gdal` Rust crate find the system libgdal.
          GDAL_HOME = "${pkgs.gdal}";
          GDAL_DATA = "${pkgs.gdal}/share/gdal";

          shellHook = ''
            echo "orbit-etl dev shell — Rust $(rustc --version)"
            echo "GDAL: $(gdalinfo --version 2>/dev/null || echo 'gdal CLI not in PATH')"
          '';
        };

        # `nix build` → builds the orbit CLI binary.
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "orbit-cli";
          version = "0.1.0";
          src = self;
          cargoLock.lockFile = "${self}/Cargo.lock";
          inherit nativeBuildInputs buildInputs;
          cargoBuildFlags = [ "-p" "orbit-cli" "--bin" "orbit" ];
          # Skip tests during nix build — they need data fixtures.
          doCheck = false;
        };

        # Optional: static-GDAL build via the `static-gdal` cargo feature.
        # NOTE: gdal-src 0.3 has its own native build requirements (similar
        # CMake fragility to lightgbm-sys). Use the dynamic build above by
        # default; only enable `static-gdal` for portable distribution.
      }
    );
}
