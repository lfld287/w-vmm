#!/bin/sh
set -eu
output=${1:-data.qcow2}
size=${2:-256M}
[ ! -e "$output" ] || { echo "Refusing to overwrite $output" >&2; exit 1; }
command -v qemu-img >/dev/null || { echo 'Install qemu for qemu-img' >&2; exit 1; }
mke2fs=${MKE2FS:-/opt/homebrew/opt/e2fsprogs/sbin/mke2fs}
[ -x "$mke2fs" ] || mke2fs=$(command -v mke2fs)
task_tmp=$(mktemp -d "${TMPDIR:-/tmp}/w-vmm-disk.XXXXXX")
trap 'rm -rf "$task_tmp"' EXIT HUP INT TERM
qemu-img create -f raw "$task_tmp/data.raw" "$size"
"$mke2fs" -q -t ext4 -F -L w-vmm-data "$task_tmp/data.raw"
qemu-img convert -f raw -O qcow2 -o compat=1.1,cluster_size=65536 "$task_tmp/data.raw" "$task_tmp/data.qcow2"
# Copy without overwriting an existing destination.
[ ! -e "$output" ] || exit 1
cp -n "$task_tmp/data.qcow2" "$output"
qemu-img check "$output"
