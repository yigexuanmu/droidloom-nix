# Consumes Droidloom's published release packages (the same unsigned pacman
# packages that the official install.sh configures) and assembles the Android
# tree the supervisor mounts.
#
# Why not build the Android partitions from source here? The upstream image
# pipeline drives a pinned AOSP 17 Soong build (227 sparse projects, its own
# prebuilt toolchains, ~120 GiB disk, multi-hour builds) and repackages three
# system partitions from Android CI artifacts that are prebuilt binaries
# upstream too. This derivation therefore downloads the release packages from
# the project's own published endpoint and verifies them the same way the
# upstream pacman workflow does: package hashes are pinned in
# nix/droidloom-packages.json, and the three base partitions are re-verified
# against the hashes Droidloom publishes in its base.json manifest.
{
  lib,
  stdenv,
  fetchurl,
  runCommand,
  zstd,
  packagesInfo,
}:
let
  info = builtins.fromJSON (builtins.readFile packagesInfo);
  version = "${info.version}-${toString info.release}";

  # The platform of the evaluation decides which published package set is
  # used; the packages exist for x86_64 and aarch64 only.
  arch =
    if stdenv.hostPlatform.isAarch64 then
      "aarch64"
    else if stdenv.hostPlatform.isx86_64 then
      "x86_64"
    else
      throw "droidloom prebuilt packages are published for x86_64 and aarch64 only";

  # pacman packages live in the project's GitHub release named "packages";
  # the GitHub Pages server only carries the repository database because
  # Pages caps file sizes below these archives.
  packageBaseUrl =
    "https://github.com/denialwm/droidloom/releases/download/packages";

  fetchPackage = component: fetchurl {
    url = "${packageBaseUrl}/droidloom-${component}-${version}-${arch}.pkg.tar.zst";
    sha256 = info.architectures.${arch}.${component}.sha256;
  };

  # pacman packages are plain zstd-compressed tarballs of a /usr tree.
  extractPackage =
    { component }:
    runCommand "droidloom-${component}-extracted-${version}" {
      src = fetchPackage component;
      nativeBuildInputs = [ zstd ];
    } ''
      mkdir -p "$out"
      tar -I zstd -xf "$src" -C "$out"
    '';

  # The activated tree mirrors the upstream install layout:
  #   usr/lib/droidloom/current -> runtime  (image package)
  #   usr/lib/droidloom/images/images/{system,system_ext,product}.img (base package)
  #   usr/lib/droidloom/images/images/vendor.raw.img (image package)
  activated =
    let
      base = extractPackage { component = "base"; };
      image = extractPackage { component = "image"; };
      partitions = info.architectures.${arch}.partitions;

      verifyPartition =
        name: hash:
        ''
          actual=$(sha256sum "$out/usr/lib/droidloom/images/images/${name}" | cut -d' ' -f1)
          if [ "$actual" != "${hash}" ]; then
            echo "partition ${name}: hash mismatch (got $actual, expected ${hash})" >&2
            exit 1
          fi
        '';
    in
    runCommand "droidloom-android-${version}" { } ''
      mkdir -p "$out"
      cp -a ${image}/usr "$out/usr"
      # The copied store paths are read-only; unlock them so the base package
      # can be merged into the same tree.
      chmod -R u+w "$out"
      cp -a ${base}/usr/. "$out/usr/"

      ${verifyPartition "system.img" partitions."system.img"}
      ${verifyPartition "system_ext.img" partitions."system_ext.img"}
      ${verifyPartition "product.img" partitions."product.img"}

      # Spot-check the pieces the cell spec mounts at startup.
      test -f "$out/usr/lib/droidloom/images/images/vendor.raw.img"
      test -f "$out/usr/lib/droidloom/current/bin/droidloom-android-init"
      test -f "$out/usr/lib/droidloom/current/framework/services.jar"
      test -f "$out/usr/lib/droidloom/current/systemui/SystemUI.apk"
      test -f "$out/usr/lib/droidloom/current/bin/android.hardware.graphics.composer3-service.droidloom"
    '';
in
assert lib.assertMsg (builtins.hasAttr arch info.architectures)
  "droidloom-packages.json has no entry for architecture ${arch}";
{
  base = extractPackage { component = "base"; };
  image = extractPackage { component = "image"; };
  runtime = extractPackage { component = "runtime"; };
  inherit activated;
}
