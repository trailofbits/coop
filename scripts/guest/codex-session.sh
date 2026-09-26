# SSH must register a systemd session even when the base image's locally
# customized common-session prevents pam-auth-update from enabling its profile.
# Preserve that stack; add only the missing session module to SSH's stack.
: "${GUEST_USER:?GUEST_USER must be set by the orchestrator}"
KEYRING_SESSION_UPGRADE=0
if [ ! -f /var/lib/coop/codex-session-v1 ] || [ ! -f /etc/tmpfiles.d/coop-codex-keyring.conf ]; then
    KEYRING_SESSION_UPGRADE=1
    # A later failure must not let the next unlock mistake a partial repair
    # for completed session support.
    rm -f /var/lib/coop/codex-session-v1
fi
if ! awk '
    $1 == "session" || $1 == "-session" {
        for (i = 2; i <= NF; i++) {
            if (substr($i, 1, 1) == "#") break
            if ($i ~ /(^|\/)pam_systemd[.]so$/) found = 1
        }
    }
    END { exit !found }
' /etc/pam.d/sshd /etc/pam.d/common-session; then
    printf '\n# coop: register SSH sessions with the systemd user manager.\nsession optional pam_systemd.so\n' >> /etc/pam.d/sshd
fi

# Firecracker's base image mounts /var/lib/systemd as tmpfs. Recreate linger
# after local filesystems mount and before logind starts on every boot.
install -d -m 755 /etc/tmpfiles.d /var/lib/systemd/linger
cat >/etc/tmpfiles.d/coop-codex-keyring.conf <<EOF
d /var/lib/systemd/linger 0755 root root -
f /var/lib/systemd/linger/$GUEST_USER 0644 root root -
EOF
chmod 644 /etc/tmpfiles.d/coop-codex-keyring.conf
touch "/var/lib/systemd/linger/$GUEST_USER"
