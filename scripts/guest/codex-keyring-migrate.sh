set -euo pipefail
: "${GUEST_USER:?GUEST_USER must be set by the orchestrator}"
# This check runs only inside the live guest, never in a host-side chroot.
# Multiple private daemons may hold different versions of the same keyring.
if keyring_pids=$(timeout 10 pgrep --uid "$GUEST_USER" --full '(^|/)gnome-keyring-daemon( |$)'); then
    if [[ "$keyring_pids" == *$'\n'* ]]; then
        echo 'Multiple keyring daemons may hold conflicting credentials. Resolve those histories before migration.' >&2
        exit 1
    fi
else
    status=$?
    if [ "$status" -ne 1 ]; then
        echo 'Could not enumerate guest keyring daemons; migration was not started.' >&2
        exit 1
    fi
fi
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends build-essential \
    dbus-user-session gnome-keyring libpam-gnome-keyring libpam0g-dev \
    libpam-systemd libsecret-tools python3 python3-dbus
