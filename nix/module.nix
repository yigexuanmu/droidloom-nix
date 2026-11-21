# NixOS integration for Droidloom.
#
# This module reproduces, declaratively, everything the upstream pacman
# installation and its root helper (droidloom-package-helper) do at first
# start:
#
#   - /etc/droidloom/cell.json        generated from packaging/cell-spec-*.json
#                                     with all /usr/lib/droidloom paths
#                                     rewritten into the Nix store
#   - /etc/droidloom/runtime.env      presenter/GPU-server environment
#   - /etc/polkit-1/rules.d/49-droidloom.rules  authorizes the desktop user
#                                     to start/stop droidloomd.service
#   - Android data directories under /var/lib/droidloom with the exact
#     ownership and modes the supervisor validates (host_storage::check_parent
#     requires root-owned ancestors without group/other write bits, and
#     host_storage::validate requires direct data/ and metadata/ directories)
#   - the droidloomd system service and the user session units from
#     packaging/systemd, with ExecStart paths pointing into the store
#   - /usr/bin compat symlinks for the paths the binaries hardcode at
#     compile time (no source modification; see usrBinCompat)
#
# On purpose this module does NOT create /usr/share/droidloom/package.json
# (the pacman-ownership marker). Its absence makes droidloomctl run in
# "source installation" mode: droidloomctl start skips the helper entirely
# and talks to the systemd units directly, which is the correct behavior on
# a declarative system.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.droidloom;

  # The Droidloom binaries (from this flake, or the overlay, or set explicitly).
  droidloom = cfg.package;
  # The activated Android tree (base partitions + vendor partition + components).
  bundle = cfg.imagePackage;

  user = config.users.users.${cfg.user};
  uidStr = toString user.uid;

  # Android's writable state lives directly on the host filesystem below this
  # directory (data/ and metadata/ are bind mounted into the cell).
  dataDir = if cfg.dataDir != null then toString cfg.dataDir else "/var/lib/droidloom/users/${uidStr}/data";

  # The privileged daemon and the session commands must find the external
  # tools they invoke by bare name (netns/veth setup, nft rules, storage
  # migration, backups, adb).
  daemonPath = with pkgs; [
    iproute2
    nftables
    iptables
    util-linux
    e2fsprogs
    rsync
    coreutils
    gnutar
    zstd
    android-tools
    systemd
  ];

  # Cell specification template for the x86_64 desktop profile. The aarch64
  # template only differs in architecture-specific library overrides and the
  # development sample user; both schemas are validated by CellSpec
  # (deny_unknown_fields) at runtime, and the x86_64 one is the shape the
  # official pacman package ships as cell-template.json.
  cellTemplate = builtins.fromJSON (
    builtins.readFile ../packaging/cell-spec-x86_64-u1000.json
  );

  # The supervisor resolves these mount sources at cell construction time.
  # The template ships the upstream FHS layout (/usr/lib/droidloom/...); on
  # NixOS those files live in the store, so every template path is rewritten
  # to its bundle location. Only the three prefixes below appear in the
  # template.
  bundleCurrent = "${bundle}/usr/lib/droidloom/current";
  bundleStorage = "${bundle}/usr/lib/droidloom/storage";
  bundleImages = "${bundle}/usr/lib/droidloom/images";
  rewritePath = path: builtins.replaceStrings
    [
      "/usr/lib/droidloom/current"
      "/usr/lib/droidloom/storage"
      "/var/lib/droidloom/images/current"
    ]
    [ bundleCurrent bundleStorage bundleImages ]
    path;

  # runtime.env, byte-for-byte what droidloom-package-helper's environment()
  # writes: the presenter validates its Denial endpoint peer against these
  # values. PEER_PID=0 is the development wildcard; PEER_UID/GID=1000 is
  # Android's AID_SYSTEM, which on the preview's shared kernel (no user
  # namespace) is literally host UID/GID 1000 - it does not refer to the
  # desktop user.
  runtimeEnv = lib.concatStringsSep "\n" (
    [
      "DROIDLOOM_HOST_PEER_PID=0"
      "DROIDLOOM_HOST_PEER_UID=1000"
      "DROIDLOOM_HOST_PEER_GID=1000"
      "DROIDLOOM_RENDER_NODE=${cfg.renderNode}"
    ]
    ++ lib.optionals (cfg.graphicsBackend == "forwarding") [
      "DROIDLOOM_GPU=${cfg.gpu}"
      # droidloom-gpu-server dlopens libvulkan.so.1. Upstream installs the
      # loader into /usr/lib, which systemd user units always search; NixOS
      # has no /usr/lib, so point the loader at the nixpkgs Vulkan loader,
      # which scans /run/opengl-driver for the vendor ICD.
      "LD_LIBRARY_PATH=${lib.makeLibraryPath [ pkgs.vulkan-loader ]}"
    ]
  ) + "\n";

  # The polkit rule droidloom-package-helper would write: the desktop user may
  # start and stop droidloomd.service (the user unit's ExecStartPre/ExecStopPost).
  polkitRule = ''
    // Generated for Droidloom's configured desktop user.
    polkit.addRule(function(action, subject) {
      if (action.id === "org.freedesktop.systemd1.manage-units" && subject.user === ${builtins.toJSON cfg.user} && action.lookup("unit") === "droidloomd.service" && (action.lookup("verb") === "start" || action.lookup("verb") === "stop")) return polkit.Result.YES;
    });
  '';

  # The generated cell specification. Order matters: the template is patched
  # with machine values, user overrides are merged in, and only then are the
  # packaged paths rewritten into the store (so overrides that reference the
  # documented /usr/lib/droidloom layout keep working).
  cellSpec =
    let
      machine = {
        host_uid = user.uid;
        data_dir = dataDir;
        runtime_dir = "/run/droidloom/cells/u${uidStr}";
        denial_socket = "/run/user/${uidStr}/droidloom/native-bridge.sock";
        render_node = cfg.renderNode;
        # The pacman template empties this list; folder sharing is expressed
        # through share_user_folders and per-user overrides.
        shared_storage_directories = [ ];
      };
      graphics = lib.optionalAttrs (cfg.graphicsBackend != "drm") {
        graphics_backend = cfg.graphicsBackend;
      };
      forwarding = lib.optionalAttrs (cfg.gpu != null) {
        gpu = cfg.gpu;
      };
      gapps = lib.optionalAttrs (cfg.gapps != null) {
        gapps_dir = toString cfg.gapps;
      };
      merged = lib.recursiveUpdate (cellTemplate // machine // graphics // forwarding // gapps) cfg.cellSpec;

      rewriteOverride = override: override // { source = rewritePath override.source; };
    in
    merged
    // {
      image_dir = rewritePath merged.image_dir;
      vendor_image = rewritePath merged.vendor_image;
      android_init = lib.mapNullable rewritePath (merged.android_init or null);
      android_file_overrides = map rewriteOverride (merged.android_file_overrides or [ ]);
      android_runtime_directories = map rewriteOverride (merged.android_runtime_directories or [ ]);
    };

  # Symlinks under /usr/bin for the absolute paths the compiled binaries
  # reference. Each entry cites the referencing code. These are created
  # through tmpfiles (declarative); the Droidloom source is not modified.
  usrBinCompatRules = [
    # runtime/droidloom-supervisor/src/{ui_mode,games_driver,apk,diagnostics}.rs
    # invoke /usr/bin/timeout and /usr/bin/nsenter to run Android commands
    # inside the cell (droidloomctl logs, crashes, install, games driver).
    "L+ /usr/bin/timeout - - - - ${pkgs.coreutils}/bin/timeout"
    "L+ /usr/bin/nsenter - - - - ${pkgs.util-linux}/bin/nsenter"
    # runtime/droidloom-supervisor/src/adb.rs: the ADB bridge binary and the
    # `env -i` / systemd-run pair that starts the user's adb server.
    "L+ /usr/bin/adb - - - - ${pkgs.android-tools}/bin/adb"
    "L+ /usr/bin/systemd-run - - - - ${pkgs.systemd}/bin/systemd-run"
    "L+ /usr/bin/env - - - - ${pkgs.coreutils}/bin/env"
    # runtime/droidloom-package-support/src/main.rs: user/account lookups and
    # the pkexec escalation of the helper (kept usable for manual setup).
    "L+ /usr/bin/getent - - - - ${pkgs.glibc}/bin/getent"
    "L+ /usr/bin/pkexec - - - - ${config.security.wrapperDir}/pkexec"
    "L+ /usr/bin/runuser - - - - ${pkgs.util-linux}/bin/runuser"
    "L+ /usr/bin/systemctl - - - - /run/current-system/sw/bin/systemctl"
    # tools/droidloom-translation-bench/src/main.rs terminates benchmark
    # workloads with /usr/bin/kill.
    "L+ /usr/bin/kill - - - - ${pkgs.util-linux}/bin/kill"
  ];

  userSessionUnits = {
    # The presenter: one Wayland window per Android task. Mirrors
    # packaging/arch/droidloom.service (the variant the pacman package
    # installs) with store paths and without the pacman-only helper call.
    droidloom = {
      description = "Droidloom Android application runtime";
      documentation = [ "https://github.com/denialwm/droidloom" ];
      partOf = [ "graphical-session.target" ];
      after = [
        "graphical-session-pre.target"
        "droidloom-audio.socket"
        "droidloom-gpu.socket"
        "droidloom-devices.socket"
      ];
      wants = [
        "droidloom-applications.service"
        "droidloom-audio.socket"
        "droidloom-gpu.socket"
        "droidloom-devices.socket"
        "droidloom-recovery.service"
      ];
      wantedBy = lib.optionals cfg.autoStart [ "graphical-session.target" ];
      unitConfig.ConditionPathExists = "/etc/droidloom/cell.json";
      environment = {
        # Bare-name calls (systemctl) from droidloomctl must resolve.
        # mkForce: nixos-unstable sets a default environment.PATH on user
        # services (nixos/modules/system/boot/systemd/user.nix); a plain
        # definition conflicts with it at the same priority.
        PATH = lib.mkForce "/run/current-system/sw/bin";
      };
      serviceConfig = {
        Type = "notify";
        NotifyAccess = "main";
        Environment = [ "DROIDLOOM_MODE=desktop" ];
        EnvironmentFile = [
          "/etc/droidloom/runtime.env"
          "-%t/droidloom/session.env"
        ];
        ExecStartPre = "/run/current-system/sw/bin/systemctl start droidloomd.service";
        ExecStart = "${droidloom}/bin/droidloom-wayland";
        ExecStartPost = "${droidloom}/bin/droidloomctl start --cell";
        ExecStopPost = [
          "-${droidloom}/bin/droidloomctl stop --cell"
          "/run/current-system/sw/bin/systemctl stop droidloomd.service"
        ];
        Restart = "on-failure";
        RestartSec = 1;
        TimeoutStartSec = 150;
        TimeoutStopSec = 150;
      };
    };

    droidloom-applications = {
      description = "Droidloom native application catalog and theme";
      partOf = [ "droidloom.service" ];
      after = [ "droidloom.service" ];
      unitConfig.ConditionPathExists = "/etc/droidloom/cell.json";
      serviceConfig = {
        ExecStart = "${droidloom}/bin/droidloom-applications";
        Restart = "on-failure";
        RestartSec = 2;
      };
    };

    droidloom-recovery = {
      description = "Droidloom Android cell crash recovery";
      bindsTo = [ "droidloom.service" ];
      partOf = [ "droidloom.service" ];
      after = [ "droidloom.service" ];
      unitConfig = {
        ConditionPathExists = "/etc/droidloom/cell.json";
        StartLimitIntervalSec = 300;
        StartLimitBurst = 5;
      };
      serviceConfig = {
        ExecStartPre = "/run/current-system/sw/bin/systemctl start droidloomd.service";
        ExecStart = "${droidloom}/bin/droidloomctl supervise --cell";
        Restart = "on-failure";
        RestartSec = "5s";
        TimeoutStartSec = 150;
        TimeoutStopSec = 10;
      };
    };

    droidloom-audio = {
      description = "Droidloom host audio broker";
      wants = [ "pipewire.socket" ];
      after = [ "pipewire.socket" ];
      partOf = [ "droidloom.service" ];
      requires = [ "droidloom-audio.socket" ];
      serviceConfig = {
        ExecStart = "${droidloom}/bin/droidloom-audio --socket %t/droidloom/audio.sock";
        EnvironmentFile = "-%h/.config/droidloom/audio.env";
        Restart = "on-failure";
        RestartSec = 1;
        NoNewPrivileges = true;
        RestrictSUIDSGID = true;
        # Filesystem namespacing in a user unit implicitly creates a user
        # namespace that hides Android's audioserver UID from SO_PEERCRED;
        # this cross-UID endpoint must keep the host user namespace, so no
        # Private* sandboxing is applied here (upstream unit comment).
      };
    };

    droidloom-gpu = {
      description = "Droidloom GPU server: Android Vulkan on the host driver";
      partOf = [ "droidloom.service" ];
      requires = [ "droidloom-gpu.socket" ];
      serviceConfig = {
        EnvironmentFile = "/etc/droidloom/runtime.env";
        ExecStart = "${droidloom}/bin/droidloom-gpu-server";
        Restart = "on-failure";
        RestartSec = 1;
        NoNewPrivileges = true;
        RestrictSUIDSGID = true;
      };
    };

    droidloom-devices = {
      description = "Droidloom device broker (haptics, camera, fingerprint, credentials, location)";
      partOf = [ "droidloom.service" ];
      requires = [ "droidloom-devices.socket" ];
      serviceConfig = {
        ExecStart = "${droidloom}/bin/droidloom-devices";
        Restart = "on-failure";
        RestartSec = 1;
        NoNewPrivileges = true;
        RestrictSUIDSGID = true;
      };
    };
  };

  userSessionSockets = {
    droidloom-audio = {
      description = "Droidloom private Android audio endpoint";
      partOf = [ "droidloom.service" ];
      socketConfig = {
        ListenSequentialPacket = [ "%t/droidloom/audio.sock" ];
        SocketMode = "0666";
        DirectoryMode = "0700";
        RemoveOnStop = true;
      };
    };

    droidloom-gpu = {
      description = "Droidloom private Android GPU forwarding endpoint";
      partOf = [ "droidloom.service" ];
      listenStreams = [ "%t/droidloom/gpu.sock" ];
      socketConfig = {
        SocketMode = "0666";
        DirectoryMode = "0700";
        RemoveOnStop = true;
      };
    };

    droidloom-devices = {
      description = "Droidloom private Android device endpoints";
      partOf = [ "droidloom.service" ];
      socketConfig = {
        ListenSequentialPacket = [
          "%t/droidloom/haptics.sock"
          "%t/droidloom/camera.sock"
          "%t/droidloom/fingerprint.sock"
          "%t/droidloom/credential.sock"
          "%t/droidloom/location.sock"
        ];
        SocketMode = "0666";
        DirectoryMode = "0700";
        RemoveOnStop = true;
      };
    };
  };
in
{
  options.services.droidloom = {
    enable = lib.mkEnableOption "Droidloom, Android applications in native Wayland windows";

    package = lib.mkOption {
      type = lib.types.package;
      description = ''
        The Droidloom host package (droidloomd, droidloomctl, the Wayland
        presenter, and the rest of the host binaries).
      '';
      default = pkgs.droidloom or (throw "services.droidloom: set services.droidloom.package explicitly, or enable the overlay from this flake (overlays.default)");
      defaultText = lib.literalExpression "pkgs.droidloom";
    };

    imagePackage = lib.mkOption {
      type = lib.types.package;
      description = ''
        The activated Android tree (base partitions, vendor partition, and
        Android components) that the supervisor mounts into the cell.
      '';
      default = pkgs.droidloom-android or (throw "services.droidloom: set services.droidloom.imagePackage explicitly, or enable the overlay from this flake (overlays.default)");
      defaultText = lib.literalExpression "pkgs.droidloom-android";
    };

    user = lib.mkOption {
      type = lib.types.str;
      description = ''
        The desktop user who owns the Android cell and runs the Wayland
        presenter. Android writes its data under
        /var/lib/droidloom/users/&#60;uid&#62; with root-owned private parents.
      '';
      example = "alice";
    };

    dataDir = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        The host directory holding Android's direct data/ and metadata/
        directories. The default is /var/lib/droidloom/users/&#60;uid&#62;/data.
        A custom value must sit below root-owned ancestors that are not
        group- or world-writable (validated by the supervisor at start).
      '';
    };

    renderNode = lib.mkOption {
      type = lib.types.path;
      default = "/dev/dri/renderD128";
      description = ''
        The DRM render node Android renders on. It is the only DRM device
        exposed to the cell.
      '';
    };

    graphicsBackend = lib.mkOption {
      type = lib.types.enum [
        "drm"
        "drm_dma_heap"
        "kgsl_dma_heap"
        "forwarding"
      ];
      default = "drm";
      description = ''
        Graphics pairing for the cell: `drm` for native Mesa rendering on
        AMD/Intel/Adreno/Mali, `drm_dma_heap` for linear system DMA-heap
        images, `kgsl_dma_heap` for Turnip/Zink on KGSL hosts, `forwarding`
        for GPUs without an Android driver (NVIDIA) through
        droidloom-gpu-server. Setting `forwarding` requires `gpu`.
      '';
    };

    gpu = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        The GPU Android renders on, as a PCI address (for example
        "0000:04:00.0"); "auto" is resolved at setup time by the upstream
        helper and is not available here, so state the PCI address. Only
        meaningful with graphicsBackend = "forwarding"; it also exports
        DROIDLOOM_GPU to the GPU server.
      '';
      example = "0000:04:00.0";
    };

    gapps = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = ''
        Optional Google apps add-on produced by the upstream
        droidloom-gapps tool (a directory containing manifest.json and
        images/). Google apps can only join a cell whose Android data has
        never booted, so enable this before the first start.
      '';
    };

    autoStart = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Link the user session units into graphical-session.target so the
        presenter starts with the graphical session. When disabled (the
        upstream default), start Droidloom with `droidloomctl start`.
      '';
    };

    enableUsrBinCompat = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Create /usr/bin symlinks for the absolute paths Droidloom's binaries
        reference at compile time (timeout, nsenter, adb, ...). The source is
        not modified; without these, droidloomctl's install/logs/crashes and
        the ADB bridge would fail on NixOS where /usr/bin is otherwise empty.
      '';
    };

    cellSpec = lib.mkOption {
      type = lib.types.attrs;
      default = { };
      description = ''
        Extra attributes merged into the generated /etc/droidloom/cell.json
        (validated by CellSpec, which rejects unknown fields). Overrides win
        over everything else. The generated paths for the packaged files
        (/usr/lib/droidloom/current, /usr/lib/droidloom/storage,
        /usr/lib/droidloom/images) are rewritten into the store after the
        merge, so template-style values keep working.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Expose droidloomctl (and the rest of the host binaries) on the
    # interactive PATH; the systemd units reference the store path
    # directly and would work without this, but the documented
    # `droidloomctl start` workflow would not.
    environment.systemPackages = [ cfg.package ];

    # The dma-heap backends allocate media buffers from the system DMA
    # heap (/dev/dma_heap/system). The heap device only appears once the
    # system_heap module is loaded (kernels commonly build it as a module
    # and nothing autoloads it), and the cell opens the device as the
    # desktop user, so load it at boot and make it group-accessible.
    boot.kernelModules = lib.mkIf
      (lib.elem cfg.graphicsBackend [ "drm_dma_heap" "kgsl_dma_heap" "forwarding" ])
      [ "system_heap" ];
    services.udev.extraRules = lib.mkIf
      (lib.elem cfg.graphicsBackend [ "drm_dma_heap" "kgsl_dma_heap" "forwarding" ])
      ''
        SUBSYSTEM=="dma_heap", KERNEL=="system", MODE="0660", GROUP="video"
      '';

    assertions = [
      {
        assertion = builtins.hasAttr cfg.user config.users.users;
        message = "services.droidloom.user '${cfg.user}' does not exist.";
      }
      {
        # The cell specification embeds the desktop user's UID and is
        # generated at evaluation time; NixOS only assigns UIDs to
        # auto-allocated users during activation, so an explicit UID is
        # required.
        assertion = (config.users.users.${cfg.user}.uid or null) != null;
        message = "services.droidloom: set an explicit uid for user '${cfg.user}' (auto-allocated UIDs are not known at evaluation time and the cell specification embeds the UID).";
      }
      {
        assertion = config.security.polkit.enable;
        message = "services.droidloom needs security.polkit.enable: the user session starts droidloomd.service through polkit.";
      }
      {
        assertion = (cfg.gpu != null) == (cfg.graphicsBackend == "forwarding");
        message = "services.droidloom: graphicsBackend 'forwarding' requires gpu (a PCI address), and gpu requires graphicsBackend 'forwarding'.";
      }
      {
        assertion =
          let
            range = cellTemplate.subordinate_uids;
          in
          user.uid < range.start || user.uid >= range.start + range.count;
        message = "services.droidloom: the desktop user's UID falls inside the subordinate UID range mapped to Android IDs (CellSpec rejects the overlap).";
      }
    ];

    warnings = lib.optional (
      !builtins.elem "video" (user.extraGroups or [ ])
    ) "services.droidloom: user '${cfg.user}' is not in the 'video' group; the Wayland presenter may not be able to open ${cfg.renderNode}."
    ++ lib.optional (!cfg.enableUsrBinCompat) "services.droidloom: enableUsrBinCompat is disabled; droidloomctl install/logs/crashes and the ADB bridge will fail because the binaries reference /usr/bin paths at compile time.";

    environment.etc = {
      "droidloom/cell.json" = {
        text = builtins.toJSON cellSpec;
        mode = "0644";
      };
      "droidloom/runtime.env" = {
        text = runtimeEnv;
        mode = "0644";
      };
      "polkit-1/rules.d/49-droidloom.rules" = {
        text = polkitRule;
        mode = "0644";
      };
    };

    # Android storage layout, matching host_storage::prepare and
    # development.rs exactly: root-owned private parents, world-traversable
    # data/ and metadata/ (Android system services must traverse /metadata
    # before init adjusts anything below it).
    systemd.tmpfiles.rules = [
      "d /var/lib/droidloom 0755 root root - -"
      "d /var/lib/droidloom/users 0755 root root - -"
      "d /var/lib/droidloom/users/${uidStr} 0700 root root - -"
      "d ${dataDir} 0700 root root - -"
      "d ${dataDir}/data 0755 root root - -"
      "d ${dataDir}/metadata 0755 root root - -"
      "d /run/droidloom/cells 0755 root root - -"
    ] ++ lib.optionals cfg.enableUsrBinCompat ([ "d /usr/bin 0755 root root - -" ] ++ usrBinCompatRules);

    # The privileged cell lifecycle daemon. Not enabled at boot (upstream
    # default): the user session starts it on demand through polkit.
    systemd.services.droidloomd = {
      description = "Droidloom Android cell lifecycle daemon";
      documentation = [ "https://github.com/denialwm/droidloom" ];
      after = [ "network.target" ];
      wantedBy = lib.mkDefault [ ];
      path = daemonPath;
      serviceConfig = {
        Type = "simple";
        ExecStart = "${droidloom}/bin/droidloomd --supervisor ${droidloom}/bin/droidloom-supervisor";
        TimeoutStopSec = "20s";
        KillMode = "mixed";
        # A kernel OOM kill of one Android process is Android's to handle;
        # the default (stop) would tear down the whole cell and every
        # Droidloom window with it.
        OOMPolicy = "continue";
        # The cell's cgroup root is the group droidloomd runs in, and
        # Android's netd attaches its BPF programs there. NixOS puts its own
        # accounting programs on the unit's group by default, which makes
        # those attaches fail, so droidloomd runs in an empty subgroup of
        # its own. Only the controllers the cell already had are delegated.
        Delegate = "memory pids";
        DelegateSubgroup = "supervisor";
        Restart = "on-failure";
        RestartSec = "1s";
        RuntimeDirectory = "droidloom";
        RuntimeDirectoryMode = "0755";
      };
    };

    systemd.user.services = userSessionUnits;
    systemd.user.sockets = userSessionSockets;
  };
}
