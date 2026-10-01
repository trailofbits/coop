#!/usr/bin/env bash
# Run after `cargo build --bin coop` on a Linux host with passwordless sudo,
# e2fsprogs, and loop-mount privileges.
set -euo pipefail

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
coop_bin=${COOP_BIN:-"$repo_dir/target/debug/coop"}
sudo -n true

data_dir=$(mktemp -d /tmp/coop-privileged-disk-XXXXXX)
cleanup() {
    for child in tmp dev/pts dev sys proc proc2 ''; do
        sudo -n umount "$data_dir/rootfs-mount${child:+/$child}" 2>/dev/null || true
    done
    sudo -n umount "$data_dir/sentinel" 2>/dev/null || true
    sudo -n rm -rf -- "$data_dir"
}
trap cleanup EXIT

chmod 0700 "$data_dir"
mkdir -m 0700 "$data_dir/images" "$data_dir/images/default" \
    "$data_dir/images/sparse" "$data_dir/instances" "$data_dir/squashfs-root" \
    "$data_dir/sentinel"
mkdir -p "$data_dir/squashfs-root"/{sys,dev/pts,tmp,etc}
ln -s "$data_dir/sentinel" "$data_dir/squashfs-root/proc"

stage="$data_dir/images/default/rootfs-template.ext4.new"
instance="$data_dir/instances/test/rootfs.ext4"
truncate -s 32M "$stage"
sudo -n chown root:root "$stage"
sudo -n chmod 0666 "$stage"
# A normal CLI call must reach the sudo wrapper and execute the same running
# coop inode through /proc, then repair this root-owned managed disk.
printf 'data_dir = "%s"\n' "$data_dir" > "$data_dir/config.toml"
"$coop_bin" --config "$data_dir/config.toml" list >/dev/null
[[ $(stat -c %a "$stage") == 600 ]]
mkdir -m 0700 "$data_dir/instances/test" "$data_dir/instances/sparse"

sudo -n "$coop_bin" __disk-op format "$data_dir" "$stage" >/dev/null
sudo -n "$coop_bin" __disk-op fsck-read "$data_dir" "$stage" >/dev/null
sudo -n "$coop_bin" __disk-op mount "$data_dir" "$stage"
sudo -n mount -t tmpfs tmpfs "$data_dir/sentinel"
if sudo -n "$coop_bin" __disk-op mount-proc "$data_dir" "$stage" 2>/dev/null; then
    echo 'guest-authored proc symlink was mounted' >&2
    exit 1
fi
if ! mountpoint -q "$data_dir/sentinel"; then
    echo 'outside sentinel lost its mount' >&2
    exit 1
fi
if sudo -n "$coop_bin" __disk-op unmount-proc "$data_dir" "$stage" 2>/dev/null; then
    echo 'guest-authored proc symlink was accepted for unmount' >&2
    exit 1
fi
if ! mountpoint -q "$data_dir/sentinel"; then
    echo 'outside sentinel was unmounted' >&2
    exit 1
fi
sudo -n umount "$data_dir/sentinel"
sudo -n "$coop_bin" __disk-op unmount "$data_dir" "$stage"

ln -s "$data_dir/sentinel" "$instance"
if sudo -n "$coop_bin" __disk-op copy "$data_dir" "$stage" "$instance" 2>/dev/null; then
    echo 'symlink disk destination was accepted' >&2
    exit 1
fi
[[ -L $instance ]]
rm "$instance"

# A prior interrupted copy can leave staging data. The next copy must reuse
# and clear it; the copied sparse image must retain its holes.
sparse_source="$data_dir/images/sparse/rootfs-template.ext4"
sparse_target="$data_dir/instances/sparse/rootfs.ext4"
truncate -s 64M "$sparse_source"
printf begin | dd of="$sparse_source" conv=notrunc status=none
printf end | dd of="$sparse_source" bs=1 seek=$((64 * 1024 * 1024 - 3)) conv=notrunc status=none
sudo -n dd if=/dev/zero of="$data_dir/instances/sparse/.coop-disk-copy.tmp" bs=1M count=1 status=none
sudo -n "$coop_bin" __disk-op copy "$data_dir" "$sparse_source" "$sparse_target"
sudo -n cmp "$sparse_source" "$sparse_target"
[[ $(stat -c %b "$sparse_target") -lt 2048 ]]
[[ ! -e $data_dir/instances/sparse/.coop-disk-copy.tmp ]]

# Rebuild with ordinary guest directories and exercise all chroot mounts twice.
rm "$data_dir/squashfs-root/proc"
mkdir "$data_dir/squashfs-root/proc"
sudo -n "$coop_bin" __disk-op format "$data_dir" "$stage" >/dev/null
for _ in 1 2; do
    sudo -n "$coop_bin" __disk-op mount "$data_dir" "$stage"
    for child in proc sys dev devpts tmp; do
        sudo -n "$coop_bin" __disk-op "mount-$child" "$data_dir" "$stage"
    done
    if sudo -n mv "$data_dir/rootfs-mount/proc" "$data_dir/rootfs-mount/proc2" 2>/dev/null; then
        echo 'active guest mountpoint was renamed' >&2
        exit 1
    fi
    sudo -n "$coop_bin" __disk-op resolv "$data_dir" "$stage"
    [[ -s "$data_dir/rootfs-mount/etc/resolv.conf" ]]
    # Guest can make its own root writable; cleanup must still work.
    sudo -n chmod 0777 "$data_dir/rootfs-mount"
    for child in tmp devpts dev sys proc; do
        sudo -n "$coop_bin" __disk-op "unmount-$child" "$data_dir" "$stage"
    done
    sudo -n "$coop_bin" __disk-op unmount "$data_dir" "$stage"
    if mountpoint -q "$data_dir/rootfs-mount"; then
        echo 'loop mount remained after unmount' >&2
        exit 1
    fi
done

sudo -n "$coop_bin" __disk-op copy "$data_dir" "$stage" "$instance"
[[ $(stat -c %a "$instance") == 600 ]]
sudo -n "$coop_bin" __disk-op truncate "$data_dir" "$instance" 1
if sudo -n "$coop_bin" __disk-op truncate "$data_dir" "$instance" 0 2>/dev/null; then
    echo 'disk shrink was accepted' >&2
    exit 1
fi
[[ $(stat -c %s "$instance") == 1073741824 ]]
sudo -n "$coop_bin" __disk-op fsck-fix "$data_dir" "$instance" >/dev/null
sudo -n "$coop_bin" __disk-op resize "$data_dir" "$instance" >/dev/null
sudo -n "$coop_bin" __disk-op swap "$data_dir" "$stage"
sudo -n "$coop_bin" __disk-op remove "$data_dir" "$instance"
[[ ! -e $instance ]]
echo 'privileged disk probe passed'
