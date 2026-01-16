{
  description = "ScalarBoard: Tensorboard Alternative for Scalars only";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {nixpkgs, ...} @ inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux"];
      perSystem = {
        config,
        system,
        ...
      }: let
        overlays = [(import inputs.rust-overlay)];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustPlatform = pkgs.makeRustPlatform {
          rustc = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default);
          cargo = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default);
        };
        gui_packages = with pkgs; [
          libGL
          libGLU
          mesa
          mesa.drivers
          gobject-introspection
          gnutls
          webkitgtk_6_0
          libsoup_3
          glib
          glib-networking
          libxml2
          gettext
          cairo
          gtk4
          gtk4-layer-shell
          egl-wayland
          wayland
          vulkan-loader
          vulkan-validation-layers
          vulkan-headers
          vulkan-tools
          libinput
          libxkbcommon
          samply
        ];
      in {
        devShells.default = pkgs.mkShell rec {
          inputsFrom = [config.packages.scalarboard];
          packages = with pkgs; [
            pre-commit
            mesa-demos
            vscode-extensions.llvm-org.lldb-vscode
            taplo
            glib-networking
          ];
          LD_LIBRARY_PATH = "/run/opengl-driver/lib:/run/opengl-driver-32/lib";
          GIO_MODULE_DIR = "${pkgs.glib-networking}/lib/gio/modules/";
          shellHook = ''
            export LD_LIBRARY_PATH="$LD_LIBRARY_PATH:${builtins.toString (pkgs.lib.makeLibraryPath gui_packages)}";
          '';
        };

        packages =
          {
            scalarboard = rustPlatform.buildRustPackage {
              pname = "scalarboard";
              version = "0.1.0";
              src = ./.;
              cargoLock = {
                lockFile = ./Cargo.lock;
                outputHashes = {
                  "egui_plot-0.34.0" = "sha256-cRypTr1mC8IcEDgQ0i+boHd4SQJ2JQWyyWwmKgqTClY=";
                };
              };
              GIO_MODULE_DIR = "${pkgs.glib-networking}/lib/gio/modules/";
              LD_LIBRARY_PATH = "/run/opengl-driver/lib:/run/opengl-driver-32/lib";

              nativeBuildInputs = with pkgs; [
                pkg-config
                wrapGAppsHook4
              ];
              buildInputs = gui_packages;
            };
          }
          // {default = config.packages.scalarboard;};

        formatter = pkgs.alejandra;
      };
    };
}
