#!/usr/bin/env bash
# In-guest probe; prints one JSON object describing machine state.
set -uo pipefail
state="$(systemctl is-system-running 2>/dev/null)"
failed="$(systemctl --failed --no-legend --plain 2>/dev/null | awk '{print $1}' | jq -R . | jq -sc .)"
jq -n \
  --arg pid1 "$(cat /proc/1/comm)" \
  --arg state "$state" \
  --argjson failed "${failed:-[]}" \
  --arg sshd "$(systemctl is-active ssh 2>/dev/null)" \
  --arg docker "$(systemctl is-active docker 2>/dev/null)" \
  --arg machine_id "$(cat /etc/machine-id 2>/dev/null)" \
  --arg hostkey "$(cut -d' ' -f1-2 /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null)" \
  --arg nproc "$(nproc)" \
  --arg mem_kb "$(awk '/MemTotal/{print $2}' /proc/meminfo)" \
  --arg kernel "$(uname -r)" \
  --arg rootfs "$(df -B1 --output=size,avail / | tail -1 | xargs)" \
  --arg cgroup "$(stat -fc %T /sys/fs/cgroup)" \
  '{pid1:$pid1,system_state:$state,failed_units:$failed,sshd:$sshd,docker:$docker,
    machine_id:$machine_id,ssh_host_key:$hostkey,nproc:($nproc|tonumber),
    mem_kb:($mem_kb|tonumber),kernel:$kernel,rootfs_size_avail:$rootfs,cgroupfs:$cgroup}'
