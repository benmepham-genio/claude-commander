{
  description = "Claude Commander - Terminal UI for managing Claude coding sessions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, crane, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.mkLib pkgs;

        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          inherit (craneLib.crateNameFromCargoToml { cargoToml = ./Cargo.toml; }) pname version;
          strictDeps = true;

          nativeBuildInputs = with pkgs; [
            pkg-config
            makeWrapper
          ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
            pkgs.apple-sdk_15
          ];
        };

        # Build only dependencies (cached separately for incremental rebuilds)
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        claude-commander = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;

          # Some tests require a real git repo, which isn't available in the Nix sandbox
          doCheck = false;

          # tmux and git are required at runtime
          postFixup = ''
            wrapProgram $out/bin/claude-commander \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.tmux pkgs.git ]}
          '';

          meta = with pkgs.lib; {
            description = "A high-performance terminal UI for managing Claude coding sessions";
            homepage = "https://github.com/sizeak/claude-commander";
            license = licenses.mit;
            mainProgram = "claude-commander";
          };
        });
      in
      {
        checks = {
          inherit claude-commander;
        };

        packages = {
          claude-commander = claude-commander;
          default = claude-commander;
        };

        apps.default = {
          type = "app";
          program = "${claude-commander}/bin/claude-commander";
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = with pkgs; [
            rust-analyzer
            tmux
            git
          ];
        };
      }
    );
}
