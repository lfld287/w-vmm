#!/usr/bin/env python3
"""Real HVF smoke tests. Creates disposable disks; never formats a supplied disk."""

import argparse
import json
import platform
import resource
import hashlib
import os
import pathlib
import pty
import select
import shutil
import signal
import subprocess
import tempfile
import termios
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
ap = argparse.ArgumentParser()
ap.add_argument("--binary", default=str(ROOT / "dist/w-vmm"))
args = ap.parse_args()
BINARY = str(pathlib.Path(args.binary).resolve())
LOGS = ROOT / "test-results"
LOGS.mkdir(exist_ok=True)


class VM:
    def __init__(self, name, opts=(), binary=BINARY, cwd=None, preexec_fn=None):
        self.master, self.slave = pty.openpty()
        self.original = termios.tcgetattr(self.slave)
        self.log = open(LOGS / (name + ".log"), "wb")
        self.buf = b""
        self.p = subprocess.Popen(
            [binary, "run", *opts],
            stdin=self.slave,
            stdout=self.slave,
            stderr=self.slave,
            cwd=cwd,
            close_fds=True,
            preexec_fn=preexec_fn,
        )

    def read(self, timeout=0.1):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                chunk = b""
            self.log.write(chunk)
            self.log.flush()
            self.buf += chunk

    def expect(self, needle, timeout=30):
        end = time.monotonic() + timeout
        while needle not in self.buf:
            self.read()
            if self.p.poll() is not None:
                raise RuntimeError(
                    f"VM exited {self.p.returncode}: {self.buf[-2000:]!r}"
                )
            if time.monotonic() > end:
                raise TimeoutError(f"Waiting for {needle!r}: {self.buf[-2000:]!r}")
        at = self.buf.index(needle)
        self.buf = self.buf[at + len(needle) :]

    def ready(self):
        self.expect(b"W-VMM ALPINE READY")
        self.expect(b"w-vmm:~# ")

    def send(self, command):
        os.write(self.master, command.encode() + b"\n")

    def command(self, command, marker):
        # Separate echo tokens prevent matching the echoed command as successful output.
        self.send(
            command
            + " && printf '\\n%s%s\\n' '"
            + marker[: len(marker) // 2]
            + "' '"
            + marker[len(marker) // 2 :]
            + "'"
        )
        self.expect(marker.encode())

    def wait_exit(self, expected_codes=(0,)):
        end = time.monotonic() + 30
        while self.p.poll() is None:
            self.read()
            if time.monotonic() > end:
                raise TimeoutError("VM did not exit")
        self.read(0)
        assert self.p.returncode in expected_codes, self.buf[-2000:]
        restored = termios.tcgetattr(self.slave)
        # Darwin sets PENDIN when tcsetattr restores ICANON; it is transient kernel state.
        restored[3] &= ~termios.PENDIN
        expected = self.original.copy()
        expected[3] &= ~termios.PENDIN
        assert restored == expected, (
            "terminal mode was not restored",
            expected,
            restored,
        )

    def poweroff(self):
        self.send("poweroff")
        self.wait_exit()

    def close(self):
        if self.p.poll() is None:
            self.p.send_signal(signal.SIGTERM)
            try:
                self.p.wait(5)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()
        self.log.close()
        os.close(self.master)
        os.close(self.slave)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def check(*cmd):
    subprocess.run(cmd, check=True)


with tempfile.TemporaryDirectory(prefix="w-vmm-smoke-") as temp:
    td = pathlib.Path(temp)
    disk = td / "data.qcow2"
    check(str(ROOT / "scripts/create-disk.sh"), str(disk))
    with VM("no-disk") as vm:
        vm.ready()
        vm.command(
            'test "$(uname -m)" = aarch64 && test "$(cat /etc/alpine-release)" = 3.24.1',
            "SHELL_PASS",
        )
        vm.poweroff()
    with VM("write", ["--disk", str(disk)]) as vm:
        vm.ready()
        blocked = subprocess.run(
            [BINARY, "run", "--disk", str(disk)], capture_output=True, timeout=10
        )
        assert blocked.returncode != 0 and b"locked" in blocked.stderr, blocked.stderr
        vm.command("mount /dev/vda /data", "MOUNT_PASS")
        # 192 KiB nonzero payload spans three 64 KiB qcow2 clusters, even when unaligned.
        vm.command(
            "dd if=/dev/urandom of=/data/payload bs=4096 count=48 && cd /data && sha256sum payload > payload.sha256 && sync && cd / && umount /data",
            "WRITE_PASS",
        )
        vm.poweroff()
    check("qemu-img", "check", str(disk))
    # Only executable and data disk present; no external boot assets or emulator process.
    offline = td / "offline"
    offline.mkdir()
    shutil.copy2(BINARY, offline / "w-vmm")
    shutil.move(disk, offline / "data.qcow2")
    disk = offline / "data.qcow2"
    assert sorted(p.name for p in offline.iterdir()) == ["data.qcow2", "w-vmm"]
    with VM(
        "offline-read", ["--disk", "data.qcow2"], str(offline / "w-vmm"), offline
    ) as vm:
        vm.ready()
        vm.command(
            "mount /dev/vda /data && cd /data && sha256sum -c payload.sha256 && cd / && umount /data",
            "READ_PASS",
        )
        vm.poweroff()
    v2 = td / "v2.qcow2"
    check(
        "qemu-img",
        "convert",
        "-f",
        "qcow2",
        "-O",
        "qcow2",
        "-o",
        "compat=0.10",
        str(disk),
        str(v2),
    )
    with VM("qcow2-v2", ["--disk", str(v2)]) as vm:
        vm.ready()
        vm.command(
            "mount /dev/vda /data && cd /data && sha256sum -c payload.sha256 && echo v2 > version && sync && cd / && umount /data",
            "V2_PASS",
        )
        vm.poweroff()
    check("qemu-img", "check", str(v2))
    before = hashlib.sha256(disk.read_bytes()).hexdigest()
    with VM("read-only", ["--disk", str(disk), "--read-only"]) as vm:
        vm.ready()
        vm.command(
            'test "$(cat /sys/block/vda/ro)" = 1 && mount -o ro /dev/vda /data && cd /data && sha256sum -c payload.sha256 && ! touch /data/forbidden',
            "RO_PASS",
        )
        vm.send("cd /; umount /data")
        vm.poweroff()
    assert hashlib.sha256(disk.read_bytes()).hexdigest() == before, (
        "read-only image changed"
    )
    for name, sig in [
        ("sigterm", signal.SIGTERM),
        ("sigint", signal.SIGINT),
        ("sighup", signal.SIGHUP),
        ("shortcut", None),
    ]:
        with VM(name) as vm:
            vm.ready()
            if sig:
                vm.p.send_signal(sig)
            else:
                os.write(vm.master, b"\x1d")
            vm.wait_exit()
    # Force a real host EFBIG on a separate disposable image, without filling the host disk.
    failing = td / "host-error.qcow2"
    shutil.copy2(disk, failing)

    def limit_file():
        signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
        resource.setrlimit(
            resource.RLIMIT_FSIZE, (failing.stat().st_size, failing.stat().st_size)
        )

    with VM("host-io-error", ["--disk", str(failing)], preexec_fn=limit_file) as vm:
        vm.ready()
        vm.command("mount /dev/vda /data", "ERROR_MOUNT_PASS")
        vm.send("dd if=/dev/zero of=/data/too-large bs=4096 count=2048; sync")
        vm.expect(b"virtio-blk request", timeout=30)
        vm.expect(b"File too large", timeout=30)
        os.write(vm.master, b"\x1d")
        vm.wait_exit((0, 1))
    with VM("reboot") as vm:
        vm.ready()
        vm.send("reboot")
        vm.wait_exit()
    corrupt = td / "corrupt.qcow2"
    corrupt.write_bytes(b"broken")
    result = subprocess.run(
        [BINARY, "run", "--disk", str(corrupt)], capture_output=True, timeout=10
    )
    assert result.returncode != 0 and b"qcow2 header" in result.stderr, result.stderr
    (LOGS / "corrupt.log").write_bytes(result.stderr)
    check("qemu-img", "check", str(disk))
print(
    "PASS: shell, persistence, offline, read-only, lock, corrupt image, real host EFBIG, reboot, signals, terminal restoration"
)

(LOGS / "summary.json").write_text(
    json.dumps(
        {
            "result": "PASS",
            "host": platform.platform(),
            "binary_sha256": hashlib.sha256(
                pathlib.Path(BINARY).read_bytes()
            ).hexdigest(),
            "cases": [
                "shell",
                "qcow2-v3-persistence",
                "qcow2-v2",
                "offline",
                "read-only",
                "exclusive-lock",
                "corrupt-image",
                "host-EFBIG",
                "reboot",
                "SIGINT",
                "SIGTERM",
                "SIGHUP",
                "Ctrl-]",
                "terminal-restoration",
                "qemu-img-check",
            ],
        },
        indent=2,
    )
    + "\n"
)
