{
  description = "SynthHires Bridge Desktop Daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };

        nativeBuildInputs = with pkgs; [
          pkg-config
          rustPlatform.cargoSetupHook
          cargo
          rustc
          wrapGAppsHook3
        ];

        buildInputs = with pkgs; [
          openssl
          glib
          gtk3
          dbus
          libayatana-appindicator
          xdotool
          libxkbcommon
          xorg.libxcb
          xorg.libX11
          xorg.libXcursor
          xorg.libXrandr
          xorg.libXi
        ];
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "synthhires-bridge";
          version = "0.1.21";
          src = ./.;
          sourceRoot = "source/apps/desktop-daemon";

          cargoLock = {
            lockFile = ./apps/desktop-daemon/Cargo.lock;
          };

          inherit nativeBuildInputs buildInputs;

          # Tests require X11 display and IPC which aren't present in nix sandbox
          doCheck = false;

          meta = with pkgs.lib; {
            description = "SynthHires Desktop Daemon — bridges the AI agent to your PC's filesystem and terminal";
            homepage = "https://github.com/planobinario/synthhires-bridge";
            license = licenses.mit;
            maintainers = [];
            mainProgram = "synthhires-bridge";
            platforms = platforms.linux;
          };
        };

        apps.default = flake-utils.lib.mkApp {
          drv = self.packages.${system}.default;
        };

        devShells.default = pkgs.mkShell {
          inherit buildInputs;
          nativeBuildInputs = nativeBuildInputs ++ [ pkgs.rust-analyzer ];
        };
      }
    ) // {
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.synthhires-bridge;
        in {
          options.services.synthhires-bridge = {
            enable = lib.mkEnableOption "SynthHires Bridge Desktop Daemon";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.default;
              description = "The synthhires-bridge package to use.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.user.services.synthhires-bridge = {
              description = "SynthHires Bridge Daemon";
              wantedBy = [ "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/synthhires-bridge run";
                Restart = "on-failure";
                RestartSec = "5s";
              };
            };
          };
        };
    };
}
