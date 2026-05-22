{
  description = "codexusage - Fast CLI reports for OpenAI Codex session usage and cost";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem =
        { pkgs, lib, ... }:
        let
          cargoToml = lib.importTOML ./Cargo.toml;
        in
        {
          packages.default = pkgs.rustPlatform.buildRustPackage {
            pname = cargoToml.package.name;
            inherit (cargoToml.package) version;

            src = lib.cleanSource ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            cargoBuildFlags = [
              "--package"
              "codexusage"
            ];
            cargoTestFlags = [
              "--package"
              "codexusage"
            ];

            doCheck = false;

            buildInputs = lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ];

            meta = {
              inherit (cargoToml.package) description homepage;
              license = lib.licenses.mit;
              mainProgram = "codexusage";
            };
          };

          devShells.default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.rust-analyzer
            ];
          };
        };
    };
}
