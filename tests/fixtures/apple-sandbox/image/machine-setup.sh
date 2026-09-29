#!/usr/bin/env bash
# Build-time guest configuration. Runs once inside `container build`.
set -euo pipefail

apt-get update
apt-get install -y --no-install-recommends \
    systemd systemd-sysv dbus openssh-server sudo curl ca-certificates gnupg \
    lsb-release iproute2 iputils-ping netcat-openbsd socat procps jq \
    iptables kmod e2fsprogs util-linux git

install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
    | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
chmod a+r /etc/apt/keyrings/docker.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu $(lsb_release -cs) stable" \
    > /etc/apt/sources.list.d/docker.list
apt-get update
apt-get install -y --no-install-recommends \
    docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# Machine identity is generated on first boot, never baked into the image.
rm -f /etc/ssh/ssh_host_*
: > /etc/machine-id
rm -f /var/lib/dbus/machine-id
ln -sf /etc/machine-id /var/lib/dbus/machine-id

cat > /etc/systemd/system/coop-test-hostkeys.service <<'UNIT'
[Unit]
Description=Generate SSH host keys on first boot
Before=ssh.service
ConditionPathExists=!/etc/ssh/ssh_host_ed25519_key

[Service]
Type=oneshot
ExecStart=/usr/bin/ssh-keygen -A

[Install]
WantedBy=multi-user.target
UNIT

systemctl set-default multi-user.target
systemctl enable coop-test-hostkeys.service ssh.service docker.service containerd.service
systemctl disable ssh.socket 2>/dev/null || true
# Units that cannot work inside a VM-backed container workload.
systemctl mask \
    systemd-udevd.service systemd-udevd-kernel.socket systemd-udevd-control.socket \
    systemd-networkd.service systemd-networkd.socket systemd-resolved.service \
    systemd-timesyncd.service getty.target console-getty.service \
    systemd-firstboot.service 2>/dev/null || true

passwd -l root
mkdir -p /root/.ssh /var/lib/coop-test /workspace
chmod 700 /root/.ssh

apt-get clean
rm -rf /var/lib/apt/lists/*
