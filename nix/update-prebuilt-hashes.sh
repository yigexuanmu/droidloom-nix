#!/usr/bin/env python3
"""Regenerate nix/droidloom-packages.json for a Droidloom release.

Run this after bumping packaging/arch/version.json upstream (or after
fast-forwarding this tree to a new Droidloom release) so the pinned
prebuilt package hashes match the published artifacts:

    ./nix/update-prebuilt-hashes.sh [version] [release]

With no arguments, the version and release are read from
packaging/arch/version.json. The script downloads each pacman package
from the project's GitHub release and records its SHA-256, and it copies
the per-partition hashes from the base.json manifests Droidloom publishes
(the same manifests the upstream build verifies partitions against).
"""

import json
import sys
import urllib.request
from pathlib import Path

PACKAGE_BASE_URL = "https://github.com/denialwm/droidloom/releases/download/packages"
BASE_JSON_URL = "https://denialwm.github.io/droidloom/{arch}/base.json"
ARCHITECTURES = ("x86_64", "aarch64")
COMPONENTS = ("runtime", "image", "base")


def fetch(url: str) -> bytes:
    with urllib.request.urlopen(url) as response:
        return response.read()


def sha256_file(path: Path) -> str:
    import hashlib

    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    repo = Path(__file__).resolve().parent.parent
    if len(sys.argv) >= 3:
        version, release = sys.argv[1], sys.argv[2]
    else:
        upstream = json.loads(
            (repo / "packaging" / "arch" / "version.json").read_text()
        )
        version, release = upstream["version"], upstream["release"]
    full_version = f"{version}-{release}"

    architectures: dict[str, dict] = {}
    for arch in ARCHITECTURES:
        entry: dict = {}
        for component in COMPONENTS:
            name = f"droidloom-{component}-{full_version}-{arch}.pkg.tar.zst"
            target = Path(__file__).resolve().parent / f".cache-{name}"
            if not target.exists():
                print(f"downloading {name} ...")
                target.write_bytes(fetch(f"{PACKAGE_BASE_URL}/{name}"))
            entry[component] = {"sha256": sha256_file(target)}
            target.unlink()
        base_manifest = json.loads(fetch(BASE_JSON_URL.format(arch=arch)))
        if base_manifest["version"] != full_version:
            print(
                f"warning: published base.json for {arch} is "
                f"{base_manifest['version']}, not {full_version}"
            )
        entry["partitions"] = {
            name: digest
            for name, digest in sorted(base_manifest["images"].items())
        }
        architectures[arch] = entry

    output = {
        "version": version,
        "release": release,
        "comment": (
            "Pinned Droidloom release packages from "
            "https://github.com/denialwm/droidloom/releases/download/packages. "
            "Regenerate with nix/update-prebuilt-hashes.sh after bumping "
            "packaging/arch/version.json."
        ),
        "architectures": architectures,
    }
    destination = repo / "nix" / "droidloom-packages.json"
    destination.write_text(json.dumps(output, indent=2) + "\n")
    print(f"wrote {destination}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
