{
  description = "Adam Audio T10S subwoofer keep-awake utility";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        rec {
          t10s-keep-awake = pkgs.rustPlatform.buildRustPackage {
            pname = "t10s_keep_awake";
            version = "0.1.0";

            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = [
              pkgs.pkg-config
            ];

            buildInputs = [
              pkgs.alsa-lib
            ];

            meta = with pkgs.lib; {
              description = "Adam Audio T10S subwoofer keep-awake utility";
              license = licenses.mit;
              platforms = platforms.linux;
              mainProgram = "t10s_keep_awake";
            };
          };

          default = t10s-keep-awake;
        }
      );

      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
            packages = with pkgs; [
              cargo
              rustc
              rust-analyzer
              clippy
              rustfmt
            ];
          };
        }
      );
    };
}
