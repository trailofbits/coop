set -euo pipefail

echo '  [guest] Configuring guest networking...'
mkdir -p /etc/systemd/network
cat > /etc/systemd/network/10-eth0.network <<'NETEOF'
[Match]
Name=eth0

[Network]
Address=172.16.0.2/24
Gateway=172.16.0.1
DNS=8.8.8.8
DNS=8.8.4.4
NETEOF

echo '  [guest] Configuring hostname...'
echo 'claude-vm' > /etc/hostname
cat > /etc/hosts <<'HOSTEOF'
127.0.0.1 localhost
127.0.1.1 claude-vm
HOSTEOF

echo '  [guest] Configuring serial console...'
mkdir -p /etc/systemd/system/serial-getty@ttyS0.service.d
cat > /etc/systemd/system/serial-getty@ttyS0.service.d/override.conf <<'GETTYEOF'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root -o '-p -- \u' --keep-baud 115200,38400,9600 %I $TERM
GETTYEOF

# Firecracker CI kernel (vmlinux-6.1.155) lacks CONFIG_NF_TABLES, so Docker's
# default iptables-nft backend fails. Switch to legacy iptables.
# Lima does NOT need this — its kernel has full nftables support.
echo '  [guest] Switching iptables to legacy (nftables not in FC kernel)...'
update-alternatives --set iptables /usr/sbin/iptables-legacy 2>/dev/null || true
update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy 2>/dev/null || true

# Firecracker CI kernel lacks systemd-resolved; the default symlink to
# 127.0.0.53 silently breaks DNS. Use a static resolv.conf instead.
# Lima uses systemd-resolved from its full Ubuntu image.
echo '  [guest] Configuring DNS (static resolv.conf)...'
rm -f /etc/resolv.conf
cat > /etc/resolv.conf <<'DNSEOF'
nameserver 8.8.8.8
nameserver 8.8.4.4
DNSEOF

# Firecracker CI kernel lacks CONFIG_IP_NF_RAW (iptable_raw module). Docker 28+
# requires the raw table for bridge networking. This env var tells Docker to skip
# raw table rules. The "insecure" label is irrelevant here — the VM's only
# network neighbor is the Firecracker host. See CLAUDE.md for full context.
# Lima does NOT need this — its kernel has the raw table module.
echo '  [guest] Configuring Docker daemon...'
mkdir -p /etc/docker
mkdir -p /etc/systemd/system/docker.service.d
cat > /etc/systemd/system/docker.service.d/no-raw.conf <<'DROPINEOF'
[Service]
Environment="DOCKER_INSECURE_NO_IPTABLES_RAW=1"
DROPINEOF

echo '  [guest] Ensuring ubuntu user exists (uid 1000)...'
if id ubuntu &>/dev/null; then
    usermod -aG sudo,docker ubuntu
else
    # Remove any other user occupying uid 1000 (e.g. Lima's host-mirror user)
    EXISTING=$(getent passwd 1000 | cut -d: -f1) || true
    if [[ -n "$EXISTING" ]]; then
        userdel "$EXISTING"
    fi
    useradd -m -s /bin/bash --uid 1000 -G sudo,docker ubuntu
fi
echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/ubuntu
chmod 440 /etc/sudoers.d/ubuntu

# Ensure home directory exists with correct ownership
mkdir -p /home/ubuntu
chown ubuntu:ubuntu /home/ubuntu
chmod 755 /home/ubuntu

# Create .local tree — the Claude Code installer expects to write here
install -d -o ubuntu -g ubuntu /home/ubuntu/.local
install -d -o ubuntu -g ubuntu /home/ubuntu/.local/bin
install -d -o ubuntu -g ubuntu /home/ubuntu/.local/share

# Copy SSH authorized_keys to ubuntu user
mkdir -p /home/ubuntu/.ssh
cp /root/.ssh/authorized_keys /home/ubuntu/.ssh/authorized_keys
chown -R ubuntu:ubuntu /home/ubuntu/.ssh
chmod 700 /home/ubuntu/.ssh
chmod 600 /home/ubuntu/.ssh/authorized_keys

echo '  [guest] Configuring ubuntu user PATH...'
echo 'export PATH="$HOME/.local/bin:$PATH"' >> /home/ubuntu/.profile
echo 'export PATH="$HOME/.local/bin:$PATH"' >> /home/ubuntu/.bashrc

echo '  [guest] Symlinking claude into system PATH...'
ln -sf /home/ubuntu/.local/bin/claude /usr/local/bin/claude

echo '  [guest] Installing claude-yolo shortcut...'
cat > /usr/local/bin/claude-yolo <<'YOLOEOF'
#!/bin/bash
exec claude --dangerously-skip-permissions "$@"
YOLOEOF
chmod 755 /usr/local/bin/claude-yolo

echo '  [guest] Installing codex-yolo shortcut...'
cat > /usr/local/bin/codex-yolo <<'YOLOEOF'
#!/bin/bash
exec codex --dangerously-bypass-approvals-and-sandbox "$@"
YOLOEOF
chmod 755 /usr/local/bin/codex-yolo

echo '  [guest] Preparing workspace directory...'
mkdir -p /workspace
chown ubuntu:ubuntu /workspace

echo '  [guest] Enabling services...'
systemctl enable docker ssh systemd-networkd serial-getty@ttyS0

echo '  [guest] Configuring SSH...'
sed -i 's/#PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config
sed -i 's/#PubkeyAuthentication.*/PubkeyAuthentication yes/' /etc/ssh/sshd_config

echo '  [guest] Configuring SSH env forwarding...'
echo 'AcceptEnv *' >> /etc/ssh/sshd_config

echo '  [guest] Setting root password for serial console debug...'
echo 'root:root' | chpasswd
