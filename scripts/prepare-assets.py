#!/usr/bin/env python3
"""Host-only, pinned Alpine -> deterministic newc initramfs (no container)."""

import gzip
import hashlib
import io
import json
import pathlib
import stat
import struct
import subprocess
import tarfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
CACHE = ROOT / ".cache"
ASSETS = ROOT / "assets"
SOURCES = [
    (
        "minirootfs.tar.gz",
        "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/aarch64/alpine-minirootfs-3.24.1-aarch64.tar.gz",
        "f55a90f69052c5bd6f92cb09a8f47065970830b194c917a006fb94028e721259",
    ),
    (
        "linux-virt.apk",
        "https://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64/linux-virt-6.18.52-r0.apk",
        "d90baf2498d5ab602145c51e9374fec039634e0087cf686c9062ac4d322e304a",
    ),
]
CACHE.mkdir(exist_ok=True)
ASSETS.mkdir(exist_ok=True)
for name, url, sha in SOURCES:
    path = CACHE / name
    if not path.exists():
        tmp = path.with_suffix(".download")
        subprocess.run(["curl", "-fL", "--retry", "3", url, "-o", str(tmp)], check=True)
        tmp.rename(path)
    if hashlib.sha256(path.read_bytes()).hexdigest() != sha:
        raise SystemExit(f"SHA-256 mismatch: {path}")


def archive(name):
    # APK v2 has concatenated gzip/tar streams; ignore zero padding between them.
    return tarfile.open(
        fileobj=io.BytesIO(gzip.decompress((CACHE / name).read_bytes())),
        mode="r:",
        ignore_zeros=True,
    )


entries = {}
with archive("minirootfs.tar.gz") as tar:
    for m in tar:
        name = m.name.removeprefix("./").rstrip("/")
        if not name or name == ".":
            continue
        if m.isfile() or m.islnk():
            data = tar.extractfile(m).read()
            mode = stat.S_IFREG | m.mode
        elif m.issym():
            data = m.linkname.encode()
            mode = stat.S_IFLNK | m.mode
        elif m.isdir():
            data = b""
            mode = stat.S_IFDIR | m.mode
        else:
            continue
        entries[name] = (mode, data)
with archive("linux-virt.apk") as tar:
    kernel = tar.extractfile("boot/vmlinuz-virt").read()
    if kernel[4:8] == b"zimg":
        offset, size = struct.unpack_from("<II", kernel, 8)
        kernel = gzip.decompress(kernel[offset : offset + size])
    elif kernel[:2] == b"\x1f\x8b":
        kernel = gzip.decompress(kernel)
    if kernel[56:60] != b"ARM\x64":
        raise SystemExit("Not an ARM64 Image")
    (ASSETS / "Image").write_bytes(kernel)
    dep = tar.extractfile("lib/modules/6.18.52-0-virt/modules.dep").read().decode()
    deps = dict(line.split(":", 1) for line in dep.splitlines())
    selected = set()

    def add(path):
        if path in selected:
            return
        selected.add(path)
        for d in deps[path].split():
            add(d)

    for mod in ["virtio_mmio", "virtio_blk", "virtio_net", "ext4"]:
        for path in deps:
            if pathlib.PurePosixPath(path).name == mod + ".ko.gz":
                add(path)
    for path in sorted(selected):
        full = "lib/modules/6.18.52-0-virt/" + path
        entries[full[:-3]] = (
            stat.S_IFREG | 0o644,
            gzip.decompress(tar.extractfile(full).read()),
        )
    entries["lib/modules/6.18.52-0-virt/modules.dep"] = (
        stat.S_IFREG | 0o644,
        "".join(
            p[:-3] + ": " + " ".join(d[:-3] for d in deps[p].split()) + "\n"
            for p in sorted(selected)
        ).encode(),
    )


def file(name, data, mode=0o644):
    entries[name] = (stat.S_IFREG | mode, data.encode())


file("init", "#!/bin/sh\nexec /sbin/init\n", 0o755)
file(
    "etc/inittab",
    "::sysinit:/etc/w-vmm-start\nttyS0::respawn:/bin/sh -l\n::ctrlaltdel:/sbin/reboot\n::shutdown:/bin/sync\n::shutdown:/bin/umount /data\n",
)
file(
    "etc/w-vmm-start",
    """#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /run /data
mount -t devpts devpts /dev/pts
hostname w-vmm
modprobe virtio_mmio
modprobe virtio_blk
modprobe virtio_net
modprobe ext4
echo 'W-VMM ALPINE READY'
echo 'Use: mount /dev/vda /data; poweroff. Host exit: Ctrl-]'
""",
    0o755,
)
file(
    "etc/profile",
    "export PATH=/usr/sbin:/usr/bin:/sbin:/bin\nexport PS1='w-vmm:\\w# '\n",
)
for name in list(entries):
    for parent in pathlib.PurePosixPath(name).parents:
        if str(parent) != ".":
            entries.setdefault(str(parent), (stat.S_IFDIR | 0o755, b""))
for name in ["proc", "sys", "dev", "data", "run"]:
    entries.setdefault(name, (stat.S_IFDIR | 0o755, b""))
out = bytearray()


def emit(name, mode, data, ino):
    encoded = name.encode() + b"\0"
    fields = [
        ino,
        mode,
        0,
        0,
        2 if stat.S_ISDIR(mode) else 1,
        0,
        len(data),
        0,
        0,
        0,
        0,
        len(encoded),
        0,
    ]
    out.extend(b"070701" + "".join(f"{n:08x}" for n in fields).encode() + encoded)
    out.extend(b"\0" * (-len(out) % 4))
    out.extend(data)
    out.extend(b"\0" * (-len(out) % 4))


for ino, (name, (mode, data)) in enumerate(sorted(entries.items()), 1):
    emit(name, mode, data, ino)
emit("TRAILER!!!", 0, b"", len(entries) + 1)
(ASSETS / "initramfs.cpio.gz").write_bytes(gzip.compress(bytes(out), mtime=0))
manifest = {
    "alpine": "3.24.1",
    "arch": "aarch64",
    "linux-virt": "6.18.52-r0",
    "sources": [{"url": u, "sha256": s} for _, u, s in SOURCES],
    "outputs": {
        n: hashlib.sha256((ASSETS / n).read_bytes()).hexdigest()
        for n in ["Image", "initramfs.cpio.gz"]
    },
}
(ASSETS / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(json.dumps(manifest, indent=2))
