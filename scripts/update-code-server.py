#!/usr/bin/env python3
"""Update the guest code-server pin; print GitHub Actions outputs on a change."""

import hashlib
import json
from pathlib import Path
import re
import sys
from urllib.request import urlopen


def main():
    path = Path(__file__).resolve().parent / "guest/web-editor.sh"
    source = path.read_text()
    versions = re.findall(r"^version=([0-9]+\.[0-9]+\.[0-9]+)$", source, re.M)
    if len(versions) != 1:
        sys.exit("Expected exactly one code-server version pin")
    current = versions[0]

    with urlopen(
        "https://api.github.com/repos/coder/code-server/releases/latest", timeout=30
    ) as response:
        release = json.load(response)
    tag = release["tag_name"]
    if (release["draft"] or release["prerelease"]
            or not re.fullmatch(r"v(0|[1-9][0-9]*)(\.(0|[1-9][0-9]*)){2}", tag)):
        sys.exit("Expected a stable code-server release with a vMAJOR.MINOR.PATCH tag")
    version = tag[1:]
    if version == current:
        return
    if tuple(map(int, version.split("."))) < tuple(map(int, current.split("."))):
        sys.exit("Refusing to downgrade code-server")

    updated = source.replace(f"version={current}\n", f"version={version}\n", 1)
    outputs = [f"previous={current}", f"version={version}"]
    for arch in ("amd64", "arm64"):
        name = f"code-server-{version}-linux-{arch}.tar.gz"
        url = f"https://github.com/coder/code-server/releases/download/{tag}/{name}"
        assets = [asset for asset in release["assets"] if asset["name"] == name]
        if (len(assets) != 1 or assets[0]["state"] != "uploaded"
                or assets[0]["browser_download_url"] != url):
            sys.exit(f"Missing or unexpected release asset: {name}")

        digest = hashlib.sha256()
        size = 0
        with urlopen(url, timeout=30) as archive:
            while chunk := archive.read(1024 * 1024):
                size += len(chunk)
                digest.update(chunk)
        checksum = digest.hexdigest()
        if size != assets[0]["size"]:
            sys.exit(f"Release asset size mismatch: {name}")
        if assets[0].get("digest") and assets[0]["digest"] != f"sha256:{checksum}":
            sys.exit(f"Release asset SHA-256 mismatch: {name}")

        updated, count = re.subn(
            rf"(\barch={arch}; checksum=)[a-f0-9]{{64}}\b",
            lambda match: match[1] + checksum,
            updated,
        )
        if count != 1:
            sys.exit(f"Expected exactly one {arch} checksum pin")
        outputs.append(f"{arch}={checksum}")

    path.write_text(updated)
    print("\n".join(outputs))


if __name__ == "__main__":
    main()
