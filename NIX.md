# Nix packaging

Nix flake for [Droidloom](https://github.com/denialwm/droidloom):
Android applications in native Wayland windows, without a VM or emulator.

This repository is the upstream Droidloom source tree plus a Nix flake in
`flake.nix` and `nix/`. The Droidloom sources are **not modified**; every
NixOS adaptation lives in the flake (generated configuration, tmpfiles,
systemd units, and compat symlinks). See `README.md` for what Droidloom
itself is.

## What is built from source, and what is not

| Part | How it is provided | Why |
| --- | --- | --- |
| Host binaries (`droidloomd`, `droidloomctl`, the Wayland presenter, the application catalog, the audio/GPU/device brokers, the doctor, and all developer tools) | **Built from source** (`nix/host-package.nix`, the full 44-crate Cargo workspace, `Cargo.lock` pinned) | Ordinary Rust; Nix builds it reproducibly |
| Android system partitions (`system.img`, `system_ext.img`, `product.img`) | **Pinned prebuilt** release packages (`nix/prebuilt-packages.nix`) | Upstream itself consumes these as pinned AOSP CI artifacts (`android/manifest/source-lock*.json`), and rebuilding the vendor partition requires a pinned AOSP 17 Soong build (227 sparse projects, its own toolchains, ~120 GiB disk, multi-hour builds) |
| Android components (patched SurfaceFlinger, Composer, HALs, SystemUI, IME, native bridge) | **Pinned prebuilt** (inside the release image package, verified at build time) | Same pipeline as above |
| Android data | **Not packaged** — created on the host filesystem under `/var/lib/droidloom` at runtime | Upstream design: no writable disk images |

Every artifact is hash-pinned:

- the pacman release packages in `nix/droidloom-packages.json` (regenerate
  with `nix/update-prebuilt-hashes.sh`),
- the three base partitions are re-verified inside the build against the
  hashes Droidloom publishes in its `base.json` manifest,
- the upstream source tree is pinned by this repository's git history.

## Quick start

Add this flake as an input, enable its overlay, and configure the module:

```nix
# flake.nix
{
  inputs.droidloom-nix.url = "github:yigexuanmu/droidloom-nix";

  outputs = { self, nixpkgs, droidloom-nix }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        {
          nixpkgs.overlays = [ droidloom-nix.overlays.default ];
        }
        droidloom-nix.nixosModules.default
        ({ lib, ... }: {
          services.droidloom = {
            enable = true;
            user = "alice";           # explicit uid required (see below)
            # graphicsBackend = "forwarding";
            # gpu = "0000:04:00.0";   # NVIDIA GPU forwarding
          };
        })
      ];
    };
  };
}
```

Then:

```console
$ nixos-rebuild switch --flake .#myhost
$ droidloomctl start          # as the desktop user, without sudo
$ droidloomctl launch com.android.settings
```

`droidloomctl start` performs the same session orchestration as upstream:
it writes `%t/droidloom/session.env`, imports the session environment into
the user manager, and starts `droidloom.service`. The user unit starts
`droidloomd.service` through polkit (authorized for the configured user
only), the presenter reports readiness to systemd, and the cell is started
once the presenter is live.

## Module options

| Option | Default | Description |
| --- | --- | --- |
| `services.droidloom.enable` | `false` | Enable the runtime, units, and configuration |
| `services.droidloom.package` | `pkgs.droidloom` | Host package (from this flake's overlay, or set explicitly) |
| `services.droidloom.imagePackage` | `pkgs.droidloom-android` | Activated Android tree (base partitions + vendor partition + components) |
| `services.droidloom.user` | — | Desktop user who owns the cell. **Must have an explicit `uid`** (NixOS assigns auto-allocated UIDs only during activation, but the cell specification embeds the UID at evaluation time) |
| `services.droidloom.dataDir` | `/var/lib/droidloom/users/<uid>/data` | Host directory with Android's direct `data/` and `metadata/` |
| `services.droidloom.renderNode` | `/dev/dri/renderD128` | The only DRM render node exposed to Android |
| `services.droidloom.graphicsBackend` | `"drm"` | `drm`, `drm_dma_heap`, `kgsl_dma_heap`, or `forwarding` (NVIDIA) |
| `services.droidloom.gpu` | `null` | PCI address of the forwarded GPU; required for `forwarding` |
| `services.droidloom.gapps` | `null` | Google apps add-on tree (from the upstream `droidloom-gapps` tool). May only join a cell whose Android data has never booted |
| `services.droidloom.autoStart` | `false` | Link the session into `graphical-session.target` |
| `services.droidloom.enableUsrBinCompat` | `true` | Create the `/usr/bin` compat symlinks (see below) |
| `services.droidloom.cellSpec` | `{ }` | Extra attributes merged into the generated `cell.json` (validated by `CellSpec`, unknown fields rejected) |

The module asserts that `security.polkit.enable` is on, that the configured
user exists with an explicit UID, and that `gpu`/`graphicsBackend` agree.
It warns when the user is not in the `video` group (the presenter opens the
render node directly).

## How the NixOS adaptation works

### Generated configuration

`/etc/droidloom/cell.json` is generated at evaluation time from the upstream
template `packaging/cell-spec-x86_64-u1000.json` (the same file the upstream
packaging pipeline ships as `cell-template.json`), with:

- `host_uid`, `data_dir`, `runtime_dir`, `denial_socket` set from the module
  configuration,
- every packaged path rewritten into the store: `/usr/lib/droidloom/current`
  and `/usr/lib/droidloom/storage` point into the image package's tree, and
  `image_dir`/`vendor_image` point into the merged image directory. The
  rewrite runs after `cellSpec` is merged, so overrides written in template
  style keep working,
- `shared_storage_directories` emptied (the pacman template does the same;
  folder sharing is expressed via `share_user_folders` and per-user
  overrides).

`/etc/droidloom/runtime.env` carries the presenter's socket peer
expectations and the render node, exactly as the upstream root helper
writes it. `PEER_UID/GID=1000` is Android's `AID_SYSTEM`, which on the
preview's shared kernel (no user namespace) is literally host UID 1000 —
it does not refer to the desktop user. When `graphicsBackend` is
`forwarding`, `DROIDLOOM_GPU` is exported for the GPU server.

`/etc/polkit-1/rules.d/49-droidloom.rules` authorizes only the configured
desktop user to start and stop `droidloomd.service` (used by the user
unit's `ExecStartPre`/`ExecStopPost`).

On purpose, the module does **not** create `/usr/share/droidloom/package.json`
(the pacman-ownership marker). Its absence puts `droidloomctl` into the
upstream "source installation" mode: it skips the pacman helper entirely and
drives the systemd units directly.

### systemd units

The system service `droidloomd.service` and the user session units mirror
`packaging/systemd/` with store paths, including:

- `Delegate = "memory pids"` and `DelegateSubgroup = "supervisor"` on
  `droidloomd` — NixOS attaches its own accounting programs to the unit's
  cgroup by default, which breaks Android's netd BPF attaches; the upstream
  unit comment calls this out, and the module keeps the delegation,
- socket-activated user endpoints for audio, GPU forwarding, and the device
  broker (`audio.sock`, `gpu.sock`, `haptics.sock`, `camera.sock`,
  `fingerprint.sock`, `credential.sock`, `location.sock`),
- the crash-recovery unit with its 5-minute circuit breaker.

The daemon is not enabled at boot (upstream default); the user session
starts it on demand.

### Android storage

`systemd.tmpfiles.rules` creates the storage layout with the exact
ownership and modes the supervisor validates: root-owned private parents
(no group/other write bits anywhere in the ancestor chain), `data/` and
`metadata/` as direct 0755 directories (Android's system services must
traverse `/metadata` before init adjusts anything below it).

### `/usr/bin` compatibility symlinks

The Droidloom binaries reference some paths under `/usr/bin` as compile-time
constants. NixOS leaves `/usr/bin` empty, so the module creates declarative
symlinks (toggle with `enableUsrBinCompat`; every entry cites the
referencing source file in `nix/module.nix`):

| Path | Provided by | Referenced by |
| --- | --- | --- |
| `/usr/bin/timeout` | coreutils | droidloomctl diagnostics, APK install, games driver |
| `/usr/bin/nsenter` | util-linux | droidloomctl diagnostics, APK install |
| `/usr/bin/adb` | android-tools | ADB bridge (`droidloomctl logs/crashes` device view) |
| `/usr/bin/systemd-run` | systemd | ADB bridge user server |
| `/usr/bin/env` | coreutils | ADB bridge, root helper |
| `/usr/bin/getent` | glibc | root helper account lookup |
| `/usr/bin/pkexec` | polkit wrapper | root helper setup/GPU selection |
| `/usr/bin/runuser` | util-linux | root helper lifecycle hooks |
| `/usr/bin/systemctl` | system systemd | root helper stop paths |
| `/usr/bin/kill` | util-linux | translation benchmark |

### External tools on the daemon's PATH

`droidloomd.service` adds `iproute2`, `nftables`, `iptables`, `util-linux`,
`e2fsprogs`, `rsync`, `coreutils`, `gnutar`, `zstd`, `android-tools`, and
`systemd` to its PATH for the bare-name invocations in the supervisor
(netns/veth setup, nft rules, storage migration, backups).

## First run and usage

```console
$ droidloomctl start        # polkit prompt on the very first start only
$ droidloomctl applications # XDG launchers exported from Android
$ droidloomctl install app.apk
$ droidloomctl launch com.example.app
$ droidloomctl status | logs | crashes | dpi | window-mode | backup | stop
```

Enable autostart by either `services.droidloom.autoStart = true;` or the
upstream `systemctl --user enable droidloom.service`.

## Differences from the Arch packages

- **Updates happen through Nix**, not pacman. Fast-forward this repository
  to the new upstream release, run `nix/update-prebuilt-hashes.sh` to re-pin
  the published packages, commit, and `nixos-rebuild switch`. Stop Droidloom
  first (`droidloomctl stop`): the pacman pre-transaction hook does the same
  check, but there is no hook to run it for you.
- `droidloomctl gpu` reports that it needs the pacman package (it rewrites
  root-owned configuration through the helper); change
  `services.droidloom.graphicsBackend`/`gpu` and rebuild instead.
- `droidloomctl update` is intentionally inert: system content is owned by
  the Nix store.
- The root helper (`droidloom-package-helper`) is still shipped and usable
  (its `pkexec`/`getent`/`systemctl` paths are among the compat symlinks),
  but the module performs its work declaratively, so `droidloomctl start`
  never invokes it.

## Updating the prebuilt package pins

```console
$ ./nix/update-prebuilt-hashes.sh            # version from packaging/arch/version.json
$ ./nix/update-prebuilt-hashes.sh 0.1.0 50   # explicit version and release
```

The script downloads each package from the release endpoint once (cached
temporarily), records its SHA-256, and copies the per-partition hashes from
the published `base.json`. Review the diff of
`nix/droidloom-packages.json`, then rebuild.

## Development

```console
$ nix develop          # Rust toolchain + the build-time native dependencies
$ nix build .#droidloom
$ nix build .#droidloom-android
$ nix flake check
```

Inspection of the generated configuration without building a system:

```console
$ nix eval --raw '.#nixosConfigurations.test.config.environment.etc."droidloom/cell.json".text'
```

Build-time native dependencies, for reference:

- `pkg-config` + `pipewire` — `droidloom-audio`'s build script compiles a
  small PipeWire client against `libpipewire-0.3`
- `glslang` — `droidloom-gpu-server`'s build script compiles the ETC2/ASTC
  compute shaders
- `mesa` — libgbm for the presenter (the `gbm` crate's default
  `import-wayland` feature also needs `wayland-server.pc`, hence `wayland`)
- `libglvnd` — EGL/GLESv2 for `droidloom-dmabuf-probe`

`drm-ffi`/`drm-sys` and the Wayland client stack are pure Rust here; no
libdrm or libwayland headers are needed for them.

## Known limitations

- Upstream preview semantics apply: the cell runs without a user namespace,
  so Android UIDs are host UIDs (that is why `AID_SYSTEM` is 1000), and
  the Droidloom threat model does not claim VM-grade isolation.
- One desktop user per host (the upstream pacman setup is single-user too).
- The full Android image cannot yet be built inside the Nix sandbox; see
  "What is built from source" above.

## License

The Droidloom sources are GPL-3.0-or-later (see `LICENSE` and
`THIRD_PARTY.md`; the Android base partitions carry their own Android
licensing, documented in the upstream packages' notices). The packaging
files added by this repository (`flake.nix`, `nix/`, `.github/`) follow the
same license.
