# Runs after the embedded helper sources have been staged in KEYRING_BUILD.
# This script installs files only. Activation waits for the next guest boot,
# retiring old private buses/keyrings and their native updaters together.
: "${GUEST_USER:?GUEST_USER must be set by the orchestrator}"
cc -std=c11 -O2 -Wall -Wextra -Werror -fstack-protector-strong \
    -D_FORTIFY_SOURCE=2 -Wl,-z,relro,-z,now \
    "$KEYRING_BUILD/pam.c" -lpam -o "$KEYRING_BUILD/pam"
install -d -m 755 /usr/local/libexec /var/lib/coop
install -m 755 "$KEYRING_BUILD/pam" /usr/local/libexec/coop-codex-keyring-pam
install -m 755 "$KEYRING_BUILD/keyring.py" /usr/local/bin/codex-keyring
cat >/etc/pam.d/coop-codex-keyring <<'PAMEOF'
# Dedicated unprivileged keyring operation; never used to authenticate a login.
# pam_exec collects PAM_AUTHTOK; pam_gnome_keyring passes it to the existing
# control socket. No auto_start: the packaged systemd service owns the daemon.
auth required pam_exec.so expose_authtok /usr/bin/true
auth required pam_gnome_keyring.so
PAMEOF
chmod 644 /etc/pam.d/coop-codex-keyring
# Ubuntu's package enables a common-password hook. Coop uses only the dedicated
# service above; explicitly remove that global hook after package installation.
DEBIAN_FRONTEND=noninteractive pam-auth-update --package --remove gnome-keyring
# Offline-safe equivalent of enabling linger; works while building in a chroot.
install -d -m 755 /var/lib/systemd/linger
touch "/var/lib/systemd/linger/$GUEST_USER"
systemctl --global add-wants default.target gnome-keyring-daemon.service
systemctl --global enable gnome-keyring-daemon.socket
# Never overwrite a migration barrier on repeated installs in the same boot.
if [ ! -f /var/lib/coop/codex-keyring-install-boot ]; then
    cat /proc/sys/kernel/random/boot_id >/var/lib/coop/codex-keyring-install-boot
fi
chmod 644 /var/lib/coop/codex-keyring-install-boot
