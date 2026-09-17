#!/bin/sh
# Build a self-contained, locally signed runnable directory.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$root"
: "${MACOSX_DEPLOYMENT_TARGET:=15.0}"
export MACOSX_DEPLOYMENT_TARGET
if [ ! -f assets/Image ] || [ ! -f assets/initramfs.cpio.gz ] || [ scripts/prepare-assets.py -nt assets/initramfs.cpio.gz ]; then
    python3 scripts/prepare-assets.py
fi
cargo build -p w-vmm-demo --bin w-vmm --release --locked --target-dir "$root/target"
mkdir -p dist
stage=$(mktemp -d "$root/dist/.build.XXXXXX")
trap 'rm -rf "$stage"' EXIT HUP INT TERM
cp target/release/w-vmm "$stage/w-vmm"
codesign --force --sign - --entitlements assets/entitlements.plist "$stage/w-vmm"
codesign --verify --strict "$stage/w-vmm"
# Never replace a disk containing user data when rebuilding.
if [ ! -e dist/data.qcow2 ]; then
    if [ -f data.qcow2 ]; then
        cp data.qcow2 "$stage/data.qcow2"
    else
        scripts/create-disk.sh "$stage/data.qcow2" 256M
    fi
    ln "$stage/data.qcow2" dist/data.qcow2
fi
cp scripts/run.sh "$stage/run.sh"
cp scripts/DIST_README.md "$stage/README.md"
cp assets/manifest.json LICENSE THIRD_PARTY.md "$stage/"
chmod +x "$stage/run.sh"
for name in w-vmm run.sh README.md manifest.json LICENSE THIRD_PARTY.md; do
    mv -f "$stage/$name" "dist/$name"
done
printf '\nBuild complete. Start with: ./dist/run.sh\n'
