#!/usr/bin/env python3
"""Reconstruct the pinned Candle ZIP compatibility patch."""

import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import tomllib
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
PATCHES = ROOT / "patches/candle"


def files(root):
    return {p.relative_to(root): p.read_bytes() for p in root.rglob("*") if p.is_file() and p.relative_to(root) != Path("Cargo.lock")}


def main():
    metadata = json.loads((PATCHES / "upstream.json").read_text())
    version = metadata["version"]
    url = f"https://static.crates.io/crates/candle-core/candle-core-{version}.crate"
    with urllib.request.urlopen(url, timeout=60) as response:
        archive = response.read()
    if hashlib.sha256(archive).hexdigest() != metadata["sha256"]:
        raise RuntimeError("Candle archive checksum differs from metadata")
    with tempfile.TemporaryDirectory(prefix="mail-candle-") as tmp:
        tree = Path(tmp)
        with tarfile.open(fileobj=io.BytesIO(archive)) as source:
            prefix = f"candle-core-{version}/"
            for member in source.getmembers():
                if (not member.isfile() or not member.name.startswith(prefix)
                        or ".." in Path(member.name).parts):
                    raise RuntimeError(f"Unsafe archive member: {member.name}")
                dest = tree / "crates/candle-core" / member.name[len(prefix):]
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_bytes(source.extractfile(member).read())
        subprocess.run(
            ["git", "apply", str(PATCHES / "0001-zip9.patch")], check=True, cwd=tree,
        )
        expected = files(tree / "crates/candle-core")
        actual = files(ROOT / "crates/candle-core")
        for name in sorted(expected.keys() | actual.keys()):
            if expected.get(name) != actual.get(name):
                raise RuntimeError(f"Vendored source differs: crates/candle-core/{name}")

    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    if manifest["patch"]["crates-io"]["candle-core"] != {"path": "crates/candle-core"}:
        raise RuntimeError("Missing local Candle patch")
    packages = tomllib.loads((ROOT / "Cargo.lock").read_text())["package"]
    candle = [p for p in packages if p["name"] == "candle-core"]
    if len(candle) != 1 or candle[0]["version"] != version or "source" in candle[0]:
        raise RuntimeError("Candle must resolve to the local patch")
    print("Candle source checksum, compatibility patch, and lockfile verified.")


if __name__ == "__main__":
    main()
