#!/usr/bin/env bash
set -euo pipefail

# Build a Firecracker-compatible rootfs ext4 image with:
#   - Ubuntu 24.04 minimal
#   - Docker CE
#   - Node.js 22 LTS + Claude Code CLI
#   - Codex CLI
#   - Python 3.13 + uv
#   - Git, build-essential, common dev tools
#   - OpenSSH server
#   - systemd init
#
# Usage: build-rootfs.sh <output-path>
#
# Requires: debootstrap, e2fsprogs, and root privileges.

OUTPUT="${1:-.coop/rootfs.ext4}"
OUTPUT_DIR="$(dirname "$OUTPUT")"
ROOTFS_SIZE_MB=4096
MOUNT_DIR="$(mktemp -d)"
CODENAME="noble"  # Ubuntu 24.04

cleanup() {
    echo "Cleaning up..."
    # Unmount in reverse order
    umount "$MOUNT_DIR/proc" 2>/dev/null || true
    umount "$MOUNT_DIR/sys" 2>/dev/null || true
    umount "$MOUNT_DIR/dev/pts" 2>/dev/null || true
    umount "$MOUNT_DIR/dev" 2>/dev/null || true
    umount "$MOUNT_DIR" 2>/dev/null || true
    rm -rf "$MOUNT_DIR"
    # Remove loop device
    if [ -n "${LOOP_DEV:-}" ]; then
        losetup -d "$LOOP_DEV" 2>/dev/null || true
    fi
}
trap cleanup EXIT

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: This script must be run as root (for debootstrap and mount)."
    echo "Usage: sudo $0 $OUTPUT"
    exit 1
fi

command -v debootstrap >/dev/null || {
    echo "ERROR: debootstrap is required. Install with: apt install debootstrap"
    exit 1
}

mkdir -p "$OUTPUT_DIR"

echo "=== Creating ext4 image ($ROOTFS_SIZE_MB MiB) ==="
dd if=/dev/zero of="$OUTPUT" bs=1M count="$ROOTFS_SIZE_MB" status=progress
mkfs.ext4 -F "$OUTPUT"

echo "=== Mounting image ==="
LOOP_DEV="$(losetup --find --show "$OUTPUT")"
mount "$LOOP_DEV" "$MOUNT_DIR"

echo "=== Running debootstrap (Ubuntu $CODENAME) ==="
debootstrap --include=systemd,systemd-sysv "$CODENAME" "$MOUNT_DIR" \
    http://archive.ubuntu.com/ubuntu

echo "=== Mounting virtual filesystems ==="
mount -t proc proc "$MOUNT_DIR/proc"
mount -t sysfs sys "$MOUNT_DIR/sys"
mount --bind /dev "$MOUNT_DIR/dev"
mount -t devpts devpts "$MOUNT_DIR/dev/pts"

echo "=== Configuring base system ==="
cat > "$MOUNT_DIR/etc/hostname" <<EOF
claude-vm
EOF

cat > "$MOUNT_DIR/etc/hosts" <<EOF
127.0.0.1 localhost
127.0.1.1 claude-vm
EOF

# Set root password (for serial console access during debugging)
chroot "$MOUNT_DIR" bash -c 'echo "root:root" | chpasswd'

# Enable serial console for Firecracker
mkdir -p "$MOUNT_DIR/etc/systemd/system/serial-getty@ttyS0.service.d"
cat > "$MOUNT_DIR/etc/systemd/system/serial-getty@ttyS0.service.d/override.conf" <<EOF
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root -o '-p -- \\u' --keep-baud 115200,38400,9600 %I \$TERM
EOF

echo "=== Installing packages ==="
chroot "$MOUNT_DIR" bash -c '
    export DEBIAN_FRONTEND=noninteractive

    # Add universe repository
    apt-get update
    apt-get install -y --no-install-recommends \
        software-properties-common
    add-apt-repository universe
    apt-get update

    # Core tools
    apt-get install -y --no-install-recommends \
        openssh-server \
        curl \
        wget \
        git \
        build-essential \
        ca-certificates \
        gnupg \
        lsb-release \
        sudo \
        iproute2 \
        iputils-ping \
        dnsutils \
        iptables \
        kmod \
        procps \
        less \
        vim-tiny \
        jq \
        unzip
'

echo "=== Installing Docker CE ==="
chroot "$MOUNT_DIR" bash -c '
    export DEBIAN_FRONTEND=noninteractive
    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
        | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
    chmod a+r /etc/apt/keyrings/docker.gpg

    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
        https://download.docker.com/linux/ubuntu $(lsb_release -cs) stable" \
        > /etc/apt/sources.list.d/docker.list

    apt-get update
    apt-get install -y --no-install-recommends \
        docker-ce \
        docker-ce-cli \
        containerd.io \
        docker-compose-plugin

    systemctl enable docker
    systemctl enable containerd
'

echo "=== Installing Node.js 22 LTS ==="
chroot "$MOUNT_DIR" bash -c '
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
    apt-get install -y --no-install-recommends nodejs
'

echo "=== Installing Claude Code CLI ==="
chroot "$MOUNT_DIR" bash -c '
    npm install -g @anthropic-ai/claude-code
'

echo "=== Installing Codex CLI ==="
chroot "$MOUNT_DIR" bash -c '
    set -euo pipefail
    case "$(uname -m)" in
        x86_64)
            asset="codex-x86_64-unknown-linux-musl.tar.gz"
            ;;
        aarch64|arm64)
            asset="codex-aarch64-unknown-linux-musl.tar.gz"
            ;;
        *)
            echo "Unsupported architecture for Codex CLI: $(uname -m)" >&2
            exit 1
            ;;
    esac

    tmpdir=$(mktemp -d)
    trap "rm -rf \"$tmpdir\"" EXIT
    archive="$tmpdir/$asset"
    bin="$tmpdir/${asset%.tar.gz}"
    curl -fsSL -o "$archive" "https://github.com/openai/codex/releases/latest/download/$asset"
    tar -xzf "$archive" -C "$tmpdir"
    test -x "$bin"
    install -m 755 "$bin" /usr/local/bin/codex
'

echo "=== Installing Python 3.13 + uv ==="
chroot "$MOUNT_DIR" bash -c '
    export DEBIAN_FRONTEND=noninteractive
    add-apt-repository -y ppa:deadsnakes/ppa
    apt-get update
    apt-get install -y --no-install-recommends \
        python3.13 \
        python3.13-venv \
        python3.13-dev

    # Install uv
    curl -LsSf https://astral.sh/uv/install.sh | sh
'

echo "=== Configuring SSH ==="
chroot "$MOUNT_DIR" bash -c '
    systemctl enable ssh

    # Allow root login with key-based auth
    sed -i "s/#PermitRootLogin.*/PermitRootLogin prohibit-password/" /etc/ssh/sshd_config
    sed -i "s/#PubkeyAuthentication.*/PubkeyAuthentication yes/" /etc/ssh/sshd_config

    mkdir -p /root/.ssh
    chmod 700 /root/.ssh
    touch /root/.ssh/authorized_keys
    chmod 600 /root/.ssh/authorized_keys
'

echo "=== Installing guest init script ==="
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [ -f "$SCRIPT_DIR/../guest/init.sh" ]; then
    cp "$SCRIPT_DIR/../guest/init.sh" "$MOUNT_DIR/usr/local/bin/coop-init"
    chmod +x "$MOUNT_DIR/usr/local/bin/coop-init"

    # Create a systemd service for the init script
    cat > "$MOUNT_DIR/etc/systemd/system/coop-init.service" <<SVCEOF
[Unit]
Description=Moat Guest Init
After=network-online.target docker.service
Wants=network-online.target docker.service

[Service]
Type=oneshot
ExecStart=/usr/local/bin/coop-init
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
SVCEOF
    chroot "$MOUNT_DIR" systemctl enable coop-init
fi

echo "=== Configuring guest networking ==="
cat > "$MOUNT_DIR/etc/systemd/network/10-eth0.network" <<EOF
[Match]
Name=eth0

[Network]
Address=172.16.0.2/24
Gateway=172.16.0.1
DNS=8.8.8.8
DNS=8.8.4.4
EOF
chroot "$MOUNT_DIR" systemctl enable systemd-networkd

echo "=== Cleaning up ==="
chroot "$MOUNT_DIR" bash -c '
    apt-get clean
    rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*
'

echo "=== Rootfs image built at $OUTPUT ==="
echo "    Size: $(du -h "$OUTPUT" | awk "{print \$1}")"
