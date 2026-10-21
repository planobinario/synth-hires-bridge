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
          pname = "synth-hires-bridge";
          # Single source of truth: the workspace version in Cargo.toml.
          # A manual copy here drifts on every bump (deb=rust vs nix).
          version = (builtins.fromTOML (builtins.readFile ./apps/desktop-daemon/Cargo.toml)).workspace.package.version;
          # Wrap the repo so the unpacked source gets a STABLE directory name
          # (`synthhires-bridge-source`): plain store-path srcs (src = ./.)
          # land under their hashed basename, which broke sourceRoot below.
          src = pkgs.runCommand "synthhires-bridge-source" { } ''
            cp -r ${./.} $out
          '';
          sourceRoot = "synthhires-bridge-source/apps/desktop-daemon";

          cargoLock = {
            lockFile = ./apps/desktop-daemon/Cargo.lock;
          };

          inherit nativeBuildInputs buildInputs;

          # GUI/egui tests need X11/IPC, absent in the nix sandbox — but the
          # protocol + core crates are headless and fast. Run exactly those
          # (same gate as apps/desktop-daemon/AGENTS.md), not nothing.
          doCheck = true;
          cargoTestFlags = [ "-p" "daemon-protocol" "-p" "daemon-core" ];

          meta = with pkgs.lib; {
            description = "SynthHires Desktop Daemon — bridges the AI agent to your PC's filesystem and terminal";
            homepage = "https://github.com/planobinario/synthhires-bridge";
            license = licenses.mit;
            maintainers = [];
            mainProgram = "synth-hires-bridge";
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
          cfg = config.services.synth-hires-bridge;
        in {
          options.services.synth-hires-bridge = {
            enable = lib.mkEnableOption "SynthHires Bridge Desktop Daemon";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.default;
              description = "The synthhires-bridge package to use.";
            };
            # Headless by default: the module targets servers/VMs as well
            # as desktops. GUI mode needs a display; set headless=false
            # only inside a graphical session.
            headless = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Run with --headless (no egui window).";
            };
            backendUrl = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              example = "https://synthhires.com";
              description = "Backend origin passed as --backend-url.";
            };
            configDir = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              description = "State dir passed as --config-dir.";
            };
            localPort = lib.mkOption {
              type = lib.types.nullOr lib.types.port;
              default = null;
              description = "Local HTTP port passed as --local-port.";
            };
          };

          config = lib.mkIf cfg.enable {
            systemd.user.services.synthhires-bridge = {
              description = "SynthHires Bridge Daemon";
              # default.target covers headless servers/VMs without a
              # graphical session; graphical-session.target covers desktops.
              wantedBy = [ "default.target" "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/synth-hires-bridge"
                  + lib.optionalString cfg.headless " --headless"
                  + lib.optionalString (cfg.backendUrl != null) " --backend-url ${cfg.backendUrl}"
                  + lib.optionalString (cfg.configDir != null) " --config-dir ${cfg.configDir}"
                  + lib.optionalString (cfg.localPort != null) " --local-port ${toString cfg.localPort}"
                  + " run";
                Restart = "always";
                RestartSec = "5s";
              };
            };
          };
        };
    };
}
