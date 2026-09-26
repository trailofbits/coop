#!/usr/bin/env bash
set -euo pipefail

# Build coop-sandbox, the macOS VM runtime that coop's `apple-container`
# feature build drives, and install it into a directory only you (or root)
# can write.
#
# coop-sandbox (macos/coop-sandbox) runs each coop instance as a persistent
# Linux VM on apple/containerization, with its own vmnet network and no host
# mounts, socket relays, published ports, or SSH-agent forwarding. This script
# builds it in release mode, signs it ad hoc with the hardened runtime and the
# one entitlement it needs (com.apple.security.virtualization), and copies it
# into PREFIX/bin, which must be owned by you or root and not writable by
# other users except the wheel or admin group. It never uses sudo.
#
# Usage:
#   scripts/build-coop-sandbox.sh [PREFIX]
#     PREFIX defaults to ~/.local/opt/coop-sandbox
#
# coop finds <PREFIX>/bin/coop-sandbox at the default PREFIX; otherwise set
#   [apple_container]
#   binary = "<PREFIX>/bin/coop-sandbox"
# See docs/backends.md. Requires Xcode (Swift 6.2+) on an Apple Silicon Mac.

case "${1:-}" in
    -h | --help)
        sed -n '4,22p' "$0" | sed -E 's/^# ?//'
        exit 0
        ;;
esac
prefix="${1:-${HOME}/.local/opt/coop-sandbox}"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "error: coop-sandbox needs an Apple Silicon Mac" >&2
    exit 1
fi
if [[ "${prefix}" != /* ]]; then
    echo "error: PREFIX must be an absolute path" >&2
    exit 1
fi

pkg="$(cd "$(dirname "$0")/../macos/coop-sandbox" && pwd)"
swift build --package-path "${pkg}" -c release --force-resolved-versions
built="$(swift build --package-path "${pkg}" -c release --show-bin-path)/coop-sandbox"

# Another user who can write here could swap the binary coop runs. Checked
# before `install -d`, which would silently reset an existing directory's mode.
if [[ -e "${prefix}/bin" ]]; then
    # coop's run-time rule (`untrusted_dir`), minus its sticky-directory
    # exception: group-writable
    # is allowed only for wheel (0) or admin (80), whose members can already
    # sudo; Homebrew's /opt/homebrew/bin is admin-writable.
    read -r bin_uid bin_gid bin_mode < <(stat -L -f '%u %g %Lp' "${prefix}/bin")
    if [[ ! -d "${prefix}/bin" ]] || [[ "${bin_uid}" != "$(id -u)" && "${bin_uid}" != 0 ]] ||
        ((8#${bin_mode} & 8#002)) ||
        { ((8#${bin_mode} & 8#020)) && [[ "${bin_gid}" != 0 && "${bin_gid}" != 80 ]]; }; then
        echo "error: ${prefix}/bin must be a directory owned by you or root, not world-writable," \
            "and group-writable only by wheel or admin" >&2
        exit 1
    fi
else
    install -d -m 0755 "${prefix}/bin"
fi
tmp="$(mktemp "${prefix}/bin/.coop-sandbox.XXXXXX")"
trap 'rm -f "${tmp}"' EXIT
cp "${built}" "${tmp}"
codesign --force --sign - --options runtime --entitlements "${pkg}/coop-sandbox.entitlements" "${tmp}"
chmod 0755 "${tmp}"
mv -f "${tmp}" "${prefix}/bin/coop-sandbox"
trap - EXIT

"${prefix}/bin/coop-sandbox" version
echo "Installed ${prefix}/bin/coop-sandbox"
