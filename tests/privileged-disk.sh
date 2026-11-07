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
printf 'data_dir = "%s"\n' "$data_dir" >"$data_dir/config.toml"
"$coop_bin" --config "$data_dir/config.toml" list >/dev/null
[[ $(stat -c %a "$stage") == 600 ]]
mkdir -m 0700 "$data_dir/instances/test" "$data_dir/instances/sparse"

sudo -n "$coop_bin" __disk-op format "$data_dir" "$stage" >/dev/null
sudo -n "$coop_bin" __disk-op fsck-read "$data_dir" "$stage" >/dev/null
sudo -n "$coop_bin" __disk-op mount "$data_dir" "$stage"
if sudo -n "$coop_bin" __disk-op copy "$data_dir" "$stage" "$instance" 2>/dev/null; then
    echo 'mounted source disk was copied' >&2
    exit 1
fi
[[ ! -e $instance ]]
if sudo -n "$coop_bin" __disk-op truncate "$data_dir" "$stage" 1 2>/dev/null; then
    echo 'mounted source disk was truncated' >&2
    exit 1
fi
[[ $(stat -c %s "$stage") == 33554432 ]]
# Holding cwd inside the mount reproduces an EBUSY unmount. A failed cleanup
# must retain both the mounted filesystem and its backing file.
if (cd "$data_dir/rootfs-mount" && sudo -n "$coop_bin" __disk-op unmount "$data_dir" "$stage") 2>/dev/null; then
    echo 'busy rootfs was unmounted' >&2
    exit 1
fi
if sudo -n "$coop_bin" __disk-op ensure-unmounted "$data_dir" "$stage" 2>/dev/null; then
    echo 'busy rootfs was reported unmounted' >&2
    exit 1
fi
if sudo -n "$coop_bin" __disk-op remove "$data_dir" "$stage" 2>/dev/null; then
    echo 'mounted backing file was removed' >&2
    exit 1
fi
[[ -f $stage ]]
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
# A killed setup can leave the root mount busy until a child exits. Recovery
# must wait for that child, unmount the old image, and permit a fresh build.
(cd "$data_dir/rootfs-mount" && touch "$data_dir/holder-ready" && sleep 1) &
holder=$!
while [[ ! -e $data_dir/holder-ready ]]; do sleep 0.01; done
sudo -n "$coop_bin" __disk-op recover-template-mount "$data_dir" "$stage"
wait "$holder"
sudo -n "$coop_bin" __disk-op ensure-unmounted "$data_dir" "$stage"

# The source is detached, but a mounted destination must still block copy.
sudo -n cp "$stage" "$instance"
sudo -n "$coop_bin" __disk-op mount "$data_dir" "$instance"
if sudo -n "$coop_bin" __disk-op copy "$data_dir" "$stage" "$instance" 2>/dev/null; then
    echo 'mounted destination disk was replaced' >&2
    exit 1
fi
[[ -f $instance ]]
sudo -n "$coop_bin" __disk-op unmount "$data_dir" "$instance"
sudo -n "$coop_bin" __disk-op remove "$data_dir" "$instance"

# A loop mount at another path is invisible to the managed mountpoint check.
mkdir -m 0700 "$data_dir/foreign-mount"
sudo -n mount -o loop "$stage" "$data_dir/foreign-mount"
if sudo -n "$coop_bin" __disk-op remove "$data_dir" "$stage" 2>/dev/null; then
    echo 'disk mounted at another path was removed' >&2
    exit 1
fi
if sudo -n "$coop_bin" __disk-op copy "$data_dir" "$stage" "$instance" 2>/dev/null; then
    echo 'disk mounted at another path was copied' >&2
    exit 1
fi
[[ -f $stage && ! -e $instance ]]
sudo -n umount "$data_dir/foreign-mount"
sudo -n "$coop_bin" __disk-op ensure-unmounted "$data_dir" "$stage"

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
# A process holding the source inode models a VM drive descriptor. Copy must
# refuse until it closes, then succeed with the identical input.
bash -c 'exec 9<"$1"; touch "$2"; sleep 30' _ "$sparse_source" "$data_dir/held-ready" &
holder=$!
for _ in $(seq 1 100); do
    [[ -e $data_dir/held-ready ]] && break
    sleep 0.01
done
[[ -e $data_dir/held-ready ]]
if sudo -n "$coop_bin" __disk-op copy "$data_dir" "$sparse_source" "$sparse_target" 2>/dev/null; then
    echo 'disk held by a process was copied' >&2
    exit 1
fi
kill "$holder"
wait "$holder" 2>/dev/null || true
sudo -n dd if=/dev/zero of="$data_dir/instances/sparse/.coop-disk-copy.tmp" bs=1M count=1 status=none
sudo -n "$coop_bin" __disk-op copy "$data_dir" "$sparse_source" "$sparse_target"
sudo -n cmp "$sparse_source" "$sparse_target"
[[ $(stat -c %b "$sparse_target") -lt 2048 ]]
[[ ! -e $data_dir/instances/sparse/.coop-disk-copy.tmp ]]

# Rebuild with ordinary guest directories and exercise all chroot mounts twice.
rm "$data_dir/squashfs-root/proc"
mkdir "$data_dir/squashfs-root/proc"
sudo -n "$coop_bin" __disk-op format "$data_dir" "$stage" >/dev/null
for iteration in 1 2; do
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
    if [[ $iteration == 1 ]]; then
        sudo -n "$coop_bin" __disk-op recover-template-mount "$data_dir" "$stage"
    else
        for child in tmp devpts dev sys proc; do
            sudo -n "$coop_bin" __disk-op "unmount-$child" "$data_dir" "$stage"
        done
        sudo -n "$coop_bin" __disk-op unmount "$data_dir" "$stage"
    fi
    if mountpoint -q "$data_dir/rootfs-mount"; then
        echo 'loop mount remained after unmount' >&2
        exit 1
    fi
done

sudo -n "$coop_bin" __disk-op copy "$data_dir" "$stage" "$instance"
[[ $(stat -c %a "$instance") == 600 ]]
sudo -n chmod 0644 "$instance"
bash -c 'exec 9<"$1"; touch "$2"; sleep 30' _ "$instance" "$data_dir/instance-holder-ready" &
instance_holder=$!
for _ in $(seq 1 100); do
    [[ -e $data_dir/instance-holder-ready ]] && break
    sleep 0.01
done
[[ -e $data_dir/instance-holder-ready ]]
if sudo -n "$coop_bin" __disk-op truncate "$data_dir" "$instance" 1 2>/dev/null; then
    echo 'disk with an open process handle was truncated' >&2
    exit 1
fi
kill "$instance_holder"
wait "$instance_holder" 2>/dev/null || true
sudo -n chmod 0600 "$instance"
[[ $(stat -c %s "$instance") == 33554432 ]]
sudo -n "$coop_bin" __disk-op truncate "$data_dir" "$instance" 1
if sudo -n "$coop_bin" __disk-op truncate "$data_dir" "$instance" 0 2>/dev/null; then
    echo 'disk shrink was accepted' >&2
    exit 1
fi
[[ $(stat -c %s "$instance") == 1073741824 ]]
# An inconsistent inode reference count requires an actual repair, whose e2fsck
# status is 1. A read-only check must reject it before repair and pass afterward.
sudo -n debugfs -w -R 'set_inode_field <2> links_count 99' "$instance" >/dev/null 2>&1
# shellcheck disable=SC2024 # Capture root tool output in a caller-owned private log.
if sudo -n "$coop_bin" __disk-op fsck-read "$data_dir" "$instance" \
    >"$data_dir/fsck-read-error" 2>&1; then
    echo 'read-only check accepted an inconsistent filesystem' >&2
    exit 1
fi
grep -Fq 'e2fsck failed with status exit status: 4' "$data_dir/fsck-read-error"

# The image grew before fsck, but ext4 did not. Retrying the same absolute size
# must repair the inode and finish the filesystem grow through the public CLI.
printf '%s\n' '{"name":"test","index":0}' >"$data_dir/instances/test/instance.json"
fs_bytes() {
    sudo -n dumpe2fs -h "$instance" 2>/dev/null |
        awk '/^Block count:/ { count = $3 } /^Block size:/ { size = $3 } END { if (!count || !size) exit 1; printf "%.0f", count * size }'
}
[[ $(fs_bytes) -lt $(stat -c %s "$instance") ]]
"$coop_bin" --config "$data_dir/config.toml" resize test --size 1G
[[ $(fs_bytes) == $(stat -c %s "$instance") ]]
sudo -n "$coop_bin" __disk-op fsck-read "$data_dir" "$instance" >/dev/null
"$coop_bin" --config "$data_dir/config.toml" resize test --size 1G
[[ $(fs_bytes) == $(stat -c %s "$instance") ]]

# A file even one block larger than the target is still a shrink request.
sudo -n truncate -s $((1073741824 + 4096)) "$instance"
if "$coop_bin" --config "$data_dir/config.toml" resize test --size 1G 2>/dev/null; then
    echo 'unaligned disk shrink was accepted' >&2
    exit 1
fi
[[ $(stat -c %s "$instance") == $((1073741824 + 4096)) ]]
sudo -n "$coop_bin" __disk-op swap "$data_dir" "$stage"
sudo -n "$coop_bin" __disk-op remove "$data_dir" "$instance"
[[ ! -e $instance ]]
echo 'privileged disk probe passed'
