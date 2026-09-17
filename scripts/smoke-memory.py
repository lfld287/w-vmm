#!/usr/bin/env python3
"""Local HVF SMP/PSCI and virtio-mem regression. Needs aarch64-linux-musl-gcc."""
import argparse
import json
import os
import pathlib
import signal
import subprocess
import tempfile
import time
from smoke import VM, LOGS, ROOT


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--binary', default=str(ROOT / 'target/debug/w-vmm'))
    args = ap.parse_args()
    binary = str(pathlib.Path(args.binary).resolve())
    LOGS.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='w-vmm-memory-') as tmp:
        td = pathlib.Path(tmp)
        payload = td / 'payload'
        payload.mkdir()
        subprocess.run(['aarch64-linux-musl-gcc', '-O2', '-static', str(ROOT / 'scripts/guest-probe.c'), '-o', str(payload / 'probe')], check=True)
        raw, disk = td / 'probe.raw', td / 'probe.qcow2'
        subprocess.run(['qemu-img', 'create', '-f', 'raw', str(raw), '64M'], check=True)
        subprocess.run([os.environ.get('MKE2FS', '/opt/homebrew/opt/e2fsprogs/sbin/mke2fs'), '-q', '-t', 'ext4', '-F', '-d', str(payload), str(raw)], check=True)
        subprocess.run(['qemu-img', 'convert', '-f', 'raw', '-O', 'qcow2', str(raw), str(disk)], check=True)
        for count in [1, 2, 4]:
            with VM(f'smp-{count}', ['--vcpus', str(count), '--disk', str(disk)], binary=binary) as vm:
                vm.ready()
                vm.command(f'test "$(getconf _NPROCESSORS_ONLN)" = {count}', f'CPUS_{count}_OK')
                if count > 1:
                    vm.command('echo 0 > /sys/devices/system/cpu/cpu1/online && test "$(cat /sys/devices/system/cpu/cpu1/online)" = 0', 'CPU_OFF_OK')
                    vm.command('echo 1 > /sys/devices/system/cpu/cpu1/online && test "$(cat /sys/devices/system/cpu/cpu1/online)" = 1', 'CPU_ON_OK')
                vm.command(f'mount /dev/vda /data && /data/probe {count} 0 && umount /data', 'PINNED_PARALLEL_OK')
                vm.poweroff()
        sock = str(td / 'control.sock')
        def control(command, *extra):
            return json.loads(subprocess.check_output([binary, command, '--socket', sock, *extra]))
        def resize(size):
            assert control('memory-set', '--requested-mib', str(size))['ok']
            end = time.monotonic() + 60
            while time.monotonic() < end:
                status = control('memory-status')['status']
                if status['driver_ready'] and status['plugged_size_mib'] == size:
                    print(status, flush=True)
                    return
                vm.read(0.05)
            raise AssertionError(status)
        def rss():
            return int(subprocess.check_output(['ps', '-o', 'rss=', '-p', str(vm.p.pid)]))
        with VM('memory-4', ['--vcpus', '4', '--disk', str(disk), '--virtio-mem-size-mib', '1024', '--virtio-mem-requested-mib', '128', '--control-socket', sock], binary=binary) as vm:
            vm.ready()
            vm.command('mount /dev/vda /data', 'MOUNT_OK')
            resize(128)
            for n in range(2):
                resize(512)
                vm.command('test "$(awk \'/MemTotal/ {print $2}\' /proc/meminfo)" -gt 950000 && /data/probe 4 640', f'MEM_WRITE_{n}_OK')
                before = rss()
                resize(0)
                vm.command('test "$(awk \'/MemTotal/ {print $2}\' /proc/meminfo)" -lt 524288', f'MEM_ZERO_{n}_OK')
                after = rss()
                print(f'Host RSS reclaimed: {before - after} KiB', flush=True)
                assert before - after > 64 * 1024, (before, after)
            resize(256)
            vm.command('umount /data', 'UNMOUNT_OK')
            vm.poweroff()
        assert not pathlib.Path(sock).exists()
        subprocess.run(['qemu-img', 'check', str(disk)], check=True)
    for name, action in [('reboot', 'reboot'), ('signal', None), ('shortcut', None)]:
        with VM('smp-' + name, ['--vcpus', '4'], binary=binary) as vm:
            vm.ready()
            if action:
                vm.send(action)
            elif name == 'signal':
                vm.p.send_signal(signal.SIGTERM)
            else:
                os.write(vm.master, b'\x1d')
            vm.wait_exit()
    print('PASS: SMP, affinity, CPU re-online, virtio-mem data and host reclamation')

if __name__ == '__main__':
    main()
