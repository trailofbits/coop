#!/usr/bin/env bash
set -euo pipefail

# Installer for coop — downloads a prebuilt binary from GitHub Releases.
#
# Usage:
#   ./install.sh                          # latest version, uses gh or GITHUB_TOKEN
#   VERSION=v0.2.1 ./install.sh           # specific version
#   INSTALL_DIR=/usr/local/bin ./install.sh

REPO="trailofbits/coop"
BINARY="coop"
BUNDLE="attestations.jsonl"
INSTALL_DIR="${INSTALL_DIR:-${HOME}/.local/bin}"

# --- helpers ----------------------------------------------------------------

die() { printf 'Error: %s\n' "$1" >&2; exit 1; }

info() { printf '  %s\n' "$1"; }

need() {
    command -v "$1" > /dev/null 2>&1 || die "'$1' is required but not found"
}

has() {
    command -v "$1" > /dev/null 2>&1
}

detect_platform() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)  OS="linux" ;;
        Darwin) OS="darwin" ;;
        *)      die "Unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64)  ARCH="x86_64" ;;
        aarch64|arm64) ARCH="aarch64" ;;
        *)             die "Unsupported architecture: $arch" ;;
    esac
}

target_triple() {
    case "${OS}-${ARCH}" in
        linux-x86_64)   echo "x86_64-unknown-linux-musl" ;;
        linux-aarch64)  echo "aarch64-unknown-linux-musl" ;;
        darwin-aarch64) echo "aarch64-apple-darwin" ;;
        *)              die "No prebuilt binary for ${OS}-${ARCH}" ;;
    esac
}

latest_version() {
    if has gh; then
        gh release view --repo "$REPO" --json tagName -q .tagName 2>/dev/null && return
    fi
    local url="https://api.github.com/repos/${REPO}/releases/latest"
    # Pass the auth header on stdin (`-H @-`) so $GITHUB_TOKEN never appears
    # on argv where it would be visible in /proc/<pid>/cmdline or `set -x`.
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        printf 'Authorization: token %s\n' "${GITHUB_TOKEN}" \
            | curl -fsSL -H @- "$url" \
            | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p'
    else
        curl -fsSL "$url" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p'
    fi
}

# Download a release asset. Tries gh first, then curl with token, then plain curl.
download_asset() {
    local filename="$1" dest="$2"

    if has gh; then
        info "Downloading ${filename} (via gh)..."
        gh release download "$VERSION" --repo "$REPO" --pattern "$filename" --dir "$(dirname "$dest")" 2>/dev/null \
            && return
        info "gh download failed, falling back to curl..."
    fi

    local url="https://github.com/${REPO}/releases/download/${VERSION}/${filename}"

    info "Downloading ${filename}..."
    # Pass the auth header on stdin (`-H @-`) so $GITHUB_TOKEN never appears
    # on argv where it would be visible in /proc/<pid>/cmdline or `set -x`.
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        printf 'Authorization: token %s\n' "${GITHUB_TOKEN}" \
            | curl -fsSL -H @- -o "$dest" "$url"
    else
        curl -fsSL -o "$dest" "$url"
    fi
}

verify_checksum() {
    local file="$1" expected="$2"
    local actual
    if command -v sha256sum > /dev/null 2>&1; then
        actual="$(sha256sum "$file" | cut -d' ' -f1)"
    elif command -v shasum > /dev/null 2>&1; then
        actual="$(shasum -a 256 "$file" | cut -d' ' -f1)"
    else
        info "Warning: no sha256sum or shasum found, skipping checksum verification"
        return 0
    fi

    if [ "$actual" != "$expected" ]; then
        die "Checksum mismatch for $(basename "$file"): expected $expected, got $actual"
    fi
}

verify_attestation() {
    local file="$1"
    if has gh; then
        info "Verifying attestation..."
        # Verify offline against the bundle published with the release. `gh
        # attestation verify` without --bundle queries the GitHub API, and gh
        # always attaches its token, so a token lacking an SSO session for the
        # org 403s on public data. The --repo identity check is still enforced.
        download_asset "$BUNDLE" "${TMPDIR}/${BUNDLE}" \
            || die "Could not download ${BUNDLE} for ${VERSION} — releases before the bundle was published cannot be verified offline; install a newer version"
        gh attestation verify "$file" --repo "$REPO" --bundle "${TMPDIR}/${BUNDLE}" \
            || die "Attestation verification failed for $(basename "$file") — refusing to install"
    else
        info "Note: \`gh\` not installed — skipped cryptographic attestation verification."
        info "The download was verified against the published \`SHA256SUMS\` checksum, which"
        info "is the same assurance level as most \`curl | bash\` installers. For end-to-end"
        info "Sigstore verification, install \`gh\` (https://cli.github.com) and re-run, or"
        info "verify manually: \`gh attestation verify <tarball> --repo ${REPO}\`."
    fi
}

# --- main -------------------------------------------------------------------

need curl
detect_platform

VERSION="${VERSION:-$(latest_version)}"
[ -n "$VERSION" ] || die "Could not determine latest version. Set VERSION= explicitly."

TRIPLE="$(target_triple)"
TARBALL="${BINARY}-${VERSION}-${TRIPLE}.tar.gz"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

printf 'Installing %s %s (%s)\n' "$BINARY" "$VERSION" "$TRIPLE"

download_asset "$TARBALL" "${TMPDIR}/${TARBALL}"
download_asset "SHA256SUMS" "${TMPDIR}/SHA256SUMS"

info "Verifying checksum..."
EXPECTED="$(grep "${TARBALL}" "${TMPDIR}/SHA256SUMS" | cut -d' ' -f1)"
[ -n "$EXPECTED" ] || die "Tarball ${TARBALL} not found in SHA256SUMS"
verify_checksum "${TMPDIR}/${TARBALL}" "$EXPECTED"

verify_attestation "${TMPDIR}/${TARBALL}"

info "Extracting..."
tar -xzf "${TMPDIR}/${TARBALL}" -C "${TMPDIR}"

info "Installing to ${INSTALL_DIR}..."
mkdir -p "$INSTALL_DIR"
EXTRACTED="${TMPDIR}/${BINARY}-${VERSION}-${TRIPLE}/${BINARY}"
[ -f "$EXTRACTED" ] || die "Binary not found in tarball"
mv "$EXTRACTED" "${INSTALL_DIR}/${BINARY}"
chmod +x "${INSTALL_DIR}/${BINARY}"

printf '\n  %s %s installed to %s/%s\n' "$BINARY" "$VERSION" "$INSTALL_DIR" "$BINARY"

# Check if INSTALL_DIR is on PATH
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        printf '\nAdd %s to your PATH:\n' "$INSTALL_DIR"
        # shellcheck disable=SC2016
        printf '  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
        ;;
esac
