set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
export DPKG_OPTIONS='--force-confnew'
APT_OPTS=(-o Dpkg::Options::=--force-confnew)

mkdir -p /var/cache/apt/archives/partial /var/lib/dpkg/updates /var/lib/dpkg/info /var/log/apt
touch /var/lib/dpkg/status 2>/dev/null || true

cat > /usr/sbin/policy-rc.d <<'POLICY'
#!/bin/sh
exit 101
POLICY
chmod +x /usr/sbin/policy-rc.d
dpkg-divert --local --rename --add /sbin/initctl 2>/dev/null || true
ln -sf /bin/true /sbin/initctl 2>/dev/null || true

# Switch apt sources from HTTP to HTTPS. Some networks block outbound port 80
# to Canonical's archive servers while port 443 works fine. HTTPS is also
# better practice (integrity + privacy of package metadata). The squashfs
# ships with http:// URIs; rewrite them before the first apt-get update.
echo '  [guest] Switching apt sources to HTTPS...'
sed -i 's|http://|https://|g' /etc/apt/sources.list.d/ubuntu.sources 2>/dev/null || true
sed -i 's|http://|https://|g' /etc/apt/sources.list 2>/dev/null || true

echo '  [guest] Updating package lists...'
apt-get update -qq
