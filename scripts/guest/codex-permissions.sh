# Default only: user/project config and desktop thread selections take precedence.
# An administrator-owned system config is never replaced by provisioning.
(
set -eu
if [ ! -e /etc/codex/config.toml ] && [ ! -L /etc/codex/config.toml ]; then
    install -d -m 755 /etc/codex
    temporary=$(mktemp /etc/codex/.coop-permissions.XXXXXX)
    trap 'rm -f "$temporary"' EXIT
    cat > "$temporary" <<'CODEXPERMISSIONSEOF'
# coop: the guest VM is the isolation boundary.
approval_policy = "never"
default_permissions = ":danger-full-access"
CODEXPERMISSIONSEOF
    chmod 644 "$temporary"
    # Publish complete contents without replacing a concurrently created file.
    ln "$temporary" /etc/codex/config.toml \
        || test -e /etc/codex/config.toml || test -L /etc/codex/config.toml
fi
)
