{
  # Nix packaging for Droidloom: Android applications in native Wayland windows.
  #
  # This flake lives inside the upstream Droidloom source tree, so the host
  # binaries are always built from the exact sources stored here. The Android
  # system partitions are consumed from Droidloom's published release packages
  # (the same unsigned pacman packages the official install.sh configures);
  # see nix/prebuilt-packages.nix and README.md for the reasoning.
  description = "Droidloom: Android applications in native Wayland windows";

  inputs = {
    # Keep this in sync with the toolchain requirements:
    # - rust-toolchain.toml pins Rust 1.93.0 (stable)
    # - workspace edition = 2024, resolver = 3 (Cargo >= 1.85)
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      # Packaged release this tree corresponds to (packaging/arch/version.json).
      droidloomVersion = builtins.fromJSON (builtins.readFile ./packaging/arch/version.json);
      versionString = "${droidloomVersion.version}-${toString droidloomVersion.release}";

      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      forAllSystems = nixpkgs.lib.genAttrs systems;
      nixpkgsFor = system: nixpkgs.legacyPackages.${system};

      # The Cargo workspace only reads these parts of the tree. Restricting
      # the source keeps edits to the packaging files (nix/, docs/, ...)
      # from invalidating the host build.
      hostSourceFilter = { lib, ... }: lib.cleanSourceWith {
        src = self;
        filter =
          path: type:
          let
            relative = lib.removePrefix (toString self + "/") (toString path);
            topDirectory = builtins.elemAt (lib.splitString "/" relative) 0;
            isWorkspaceDirectory = builtins.elem topDirectory [
              "android"
              "runtime"
              "graphics"
              "tools"
              # Read by include_str! from the workspace (build docs, packaging
              # hooks) - see tools/droidloom-gapps/src/package.rs.
              "docs"
              "packaging"
              "protocol" # Wayland protocol XML read by wayland-scanner macros
            ];
          in
          # Root-level Cargo inputs and install.sh (included by the gapps
          # packaging tool), or anything inside a workspace directory.
          builtins.elem relative [
            "Cargo.toml"
            "Cargo.lock"
            "rust-toolchain.toml"
            "install.sh"
          ] || isWorkspaceDirectory;
      };
    in
    {
      # Expose the packages on <nixpkgs> so module users can write
      # nixpkgs.overlays = [ droidloom-nix.overlays.default ];
      overlays.default =
        final: _prev: {
          droidloom = self.packages.${final.system}.droidloom;
          droidloom-android = self.packages.${final.system}.droidloom-android;
        };

      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgsFor system;
          callPackage = pkgs.callPackage;

          # Pinned versions and hashes of Droidloom's published packages.
          prebuilt = callPackage ./nix/prebuilt-packages.nix {
            packagesInfo = ./nix/droidloom-packages.json;
          };

          host = callPackage ./nix/host-package.nix {
            src = hostSourceFilter { lib = nixpkgs.lib; };
            version = versionString;
          };
        in
        {
          # Host binaries (droidloomd, droidloomctl, presenter, catalog, ...)
          # built from the source tree this flake lives in.
          droidloom = host;

          # Extracted upstream release packages.
          droidloom-android-base = prebuilt.base;
          droidloom-android-image = prebuilt.image;
          droidloom-runtime-prebuilt = prebuilt.runtime;

          # The activated Android tree the supervisor mounts: upstream image
          # package (Android components + vendor partition) merged with the
          # upstream base package (system/system_ext/product partitions),
          # with partition hashes verified against the published base.json.
          droidloom-android = prebuilt.activated;

          default = host;
        }
      );

      nixosModules.default = import ./nix/module.nix;

      # Evaluation-only example configuration, useful for inspecting the
      # generated files without building a whole system:
      #   nix eval .#nixosConfigurations.test.config.environment.etc."droidloom/cell.json".text
      nixosConfigurations.test = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          self.nixosModules.default
          (
            { pkgs, ... }:
            {
              users.users.alice = {
                isNormalUser = true;
                uid = 1000;
                extraGroups = [ "video" ];
              };
              security.polkit.enable = true;
              services.droidloom = {
                enable = true;
                user = "alice";
                package = self.packages.${pkgs.system}.droidloom;
                imagePackage = self.packages.${pkgs.system}.droidloom-android;
              };
              # Minimal bootable skeleton so `nix flake check` can build the
              # system closure; the Droidloom module itself is what is under
              # test.
              fileSystems."/" = {
                device = "/dev/vda";
                fsType = "ext4";
              };
              boot.loader.grub.devices = [ "/dev/vda" ];
            }
          )
        ];
      };

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgsFor system;
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
              pkg-config
              glslang # droidloom-gpu-server build.rs
              pipewire # droidloom-audio build.rs
              mesa # libgbm (gbm-sys)
              libglvnd # EGL/GLESv2 (droidloom-dmabuf-probe)
            ];
          };
        }
      );

      # `nix flake check` builds the host package and the activated Android
      # tree, which is the complete packaging pipeline minus the NixOS module
      # evaluation (covered by nixosConfigurations.test).
      checks = forAllSystems (system: {
        droidloom = self.packages.${system}.droidloom;
        droidloom-android = self.packages.${system}.droidloom-android;
      });
    };
}
