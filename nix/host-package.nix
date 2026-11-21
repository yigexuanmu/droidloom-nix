# Builds every host binary of the Droidloom workspace from the source tree
# this flake lives in.
#
# What gets built: the full Cargo workspace (44 members), which includes the
# nine runtime binaries upstream installs (droidloomd, droidloomctl,
# droidloom-supervisor, droidloom-wayland, droidloom-applications,
# droidloom-audio, droidloom-gpu-server, droidloom-devices, droidloom-doctor),
# the privileged helper (droidloom-package-helper), and the developer tools
# (droidloom-update, droidloom-package, droidloom-source, droidloom-image,
# droidloom-mesa, droidloom-gapps, and the measurement/probe utilities).
# The Android-side Rust crates (Vulkan driver, HAL logic, window policy) are
# ordinary Linux crates at build time; their Android deployment happens in the
# upstream image pipeline, not here.
{
  lib,
  rustPlatform,
  pkg-config,
  glslang,
  pipewire,
  libgbm,
  wayland,
  libglvnd,
  libxkbcommon,
  libdrm,
  src,
  version,
}:
rustPlatform.buildRustPackage {
  pname = "droidloom";
  inherit version src;

  # Cargo.lock is complete (239 packages, all registry dependencies carry
  # checksums, no git dependencies, no [patch] section), so vendoring works
  # without outputHashes.
  cargoLock.lockFile = "${src}/Cargo.lock";

  nativeBuildInputs = [
    # runtime/droidloom-audio/build.rs probes libpipewire-0.3 through
    # pkg-config and compiles its small PipeWire C client.
    pkg-config
    # runtime/droidloom-gpu-server/build.rs invokes glslangValidator to
    # compile the ETC2/ASTC compute shaders for GPU forwarding.
    glslang
  ];

  buildInputs = [
    # PipeWire client library for the host audio broker.
    pipewire
    # libgbm for gbm-sys: the presenter allocates its Wayland render targets
    # through GBM (nixpkgs ships it as the separate libgbm package). The gbm
    # crate's default "import-wayland" feature also pulls wayland-backend
    # with server_system, which needs wayland-server.pc and links
    # libwayland-server - the same reason the upstream pacman package lists
    # wayland in its dependencies. (drm-ffi/drm-sys are pure Rust.)
    libgbm
    wayland
    # libEGL/libGLESv2 for the droidloom-dmabuf-probe tool.
    libglvnd
    # libxkbcommon for the presenter's keymap handling: smithay-client-toolkit
    # builds with its "xkbcommon" feature, which pkg-config-probes the C
    # library (same dependency as the upstream pacman package).
    libxkbcommon
    # libdrm headers for the droidloom-dmabuf-probe C shim
    # (#include <libdrm/drm.h>). The drm Rust crates stay pure Rust.
    libdrm
  ];

  # The upstream test suite needs a D-Bus session bus, a graphical session,
  # and systemd tooling; it is exercised on real devices, not in the build
  # sandbox.
  doCheck = false;

  meta = {
    description = "Android applications in native Wayland windows (host runtime)";
    longDescription = ''
      Droidloom runs the Android userspace (ART, Binder, Zygote,
      system_server) directly on the Linux kernel in private namespaces and
      presents each Android task as an ordinary Wayland window. Linux owns the
      display, the physical input devices, and every hardware service.
    '';
    homepage = "https://github.com/denialwm/droidloom";
    license = lib.licenses.gpl3Plus;
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    mainProgram = "droidloomctl";
  };
}
