#!/usr/bin/env python3
"""Local signed-HVF lifecycle regression; only disposable test disks are used."""
import argparse
import concurrent.futures
import json
import os
import pathlib
import signal
import socket
import subprocess
import tempfile
import time
from smoke import VM, LOGS, ROOT


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default=str(ROOT / 'target/debug/w-vmm'))
    binary = str(pathlib.Path(parser.parse_args().binary).resolve())
    LOGS.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='w-vmm-control-') as tmp:
        disk = pathlib.Path(tmp) / 'test.qcow2'
        sock = pathlib.Path(tmp) / 'control.sock'
        subprocess.run([str(ROOT / 'scripts/create-disk.sh'), str(disk)], check=True)

        def request(command, *extra):
            response = subprocess.run([binary, command, '--socket', str(sock), *extra], capture_output=True, timeout=30)
            result = json.loads(response.stdout)
            assert result['ok'], result
            return result['status']

        for cpus in [1, 4]:
            with VM(f'control-{cpus}', ['--vcpus', str(cpus), '--disk', str(disk), '--control-socket', str(sock)], binary=binary) as vm:
                vm.ready()
                vm.command('mount /dev/vda /data && echo persisted > /data/control && sync && umount /data', 'DISK_READY')
                if cpus > 1:
                    vm.command('echo 0 > /sys/devices/system/cpu/cpu1/online', 'OFFLINE_OK')
                vm.command('mount /dev/vda /data', 'LOAD_MOUNT_OK')
                vm.send('while :; do echo heartbeat > /data/live; sync; echo heartbeat; sleep 0.02; done & worker=$!')
                vm.expect(b'heartbeat\r\n')
                for _ in range(20):
                    assert request('pause')['lifecycle'] == 'Paused'
                    assert request('pause')['lifecycle'] == 'Paused'
                    assert request('status')['lifecycle'] == 'Paused'
                    vm.read(0.05)  # Drain output written before the barrier.
                    before = vm.buf
                    vm.read(0.05)
                    assert vm.buf == before, 'guest output advanced while paused'
                    assert request('resume')['lifecycle'] == 'Running'
                    assert request('resume')['lifecycle'] == 'Running'
                vm.command('kill "$worker" && wait "$worker" 2>/dev/null; sync && umount /data', 'WORKER_DONE')
                if cpus > 1:
                    vm.command('echo 1 > /sys/devices/system/cpu/cpu1/online', 'ONLINE_OK')
                assert request('pause')['lifecycle'] == 'Paused'
                vm.send("printf '%s%s\\n' 'BUFFERED_' 'INPUT_OK'")
                vm.read(0.1)
                assert b'BUFFERED_INPUT_OK' not in vm.buf
                with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                    assert all(s['lifecycle'] == 'Paused' for s in pool.map(lambda _: request('status'), range(8)))
                request('resume')
                vm.expect(b'BUFFERED_INPUT_OK')
                request('pause')
                assert request('stop')['lifecycle'] == 'Stopped'
                vm.wait_exit()
            assert not sock.exists()
            subprocess.run(['qemu-img', 'check', str(disk)], check=True)

        with VM('control-memory', ['--vcpus', '4', '--virtio-mem-size-mib', '256', '--control-socket', str(sock)], binary=binary) as vm:
            vm.ready()
            request('pause')
            before = request('memory-status')['plugged_size_mib']
            assert request('memory-set', '--requested-mib', '128')['requested_size_mib'] == 128
            time.sleep(0.1)
            assert request('memory-status')['plugged_size_mib'] == before
            request('resume')
            deadline = time.monotonic() + 60
            while request('memory-status')['plugged_size_mib'] != 128:
                assert time.monotonic() < deadline, 'memory target did not converge'
                vm.read(0.05)
            request('pause')
            request('stop')
            vm.wait_exit()
        assert not sock.exists()

        for paused in [False, True]:
            with VM(f'control-escape-{paused}', ['--control-socket', str(sock)], binary=binary) as vm:
                vm.ready()
                if paused:
                    request('pause')
                os.write(vm.master, b'\x1d')
                if paused:
                    vm.read(0.2)
                    assert vm.p.poll() is None, 'paused Ctrl-] was consumed'
                    assert request('status')['lifecycle'] == 'Paused'
                    request('resume')
                vm.wait_exit()
            assert not sock.exists()

        with VM('control-poweroff', ['--control-socket', str(sock)], binary=binary) as vm:
            vm.ready()
            vm.poweroff()
        assert not sock.exists()

        with VM('control-startup-failure', ['--memory-mib', '0', '--control-socket', str(sock)], binary=binary) as vm:
            vm.wait_exit(expected_codes=(1,))
        assert not sock.exists()

        # Failure after Terminal construction but before run must also join.
        with VM('control-early-return', ['--control-socket', str(pathlib.Path(tmp) / 'missing' / 'control.sock')], binary=binary) as vm:
            vm.wait_exit(expected_codes=(1,))

        with VM('control-concurrent-stop', ['--control-socket', str(sock)], binary=binary) as vm:
            vm.ready()
            request('pause')
            # Queue the socket request before signalling; both stops wait for
            # the same cleanup. Keep the connection open to receive its reply.
            with socket.socket(socket.AF_UNIX) as client:
                client.connect(str(sock))
                client.settimeout(30)
                # A status round-trip ensures the server accepted this
                # connection before either stop can tear down the listener.
                request('status')
                client.sendall(b'{"command":"stop"}\n')
                vm.p.send_signal(signal.SIGTERM)
                reply = client.makefile('rb').readline()
                assert json.loads(reply)['ok'], reply
            vm.wait_exit()
        assert not sock.exists()

        for sig in [signal.SIGTERM, signal.SIGINT, signal.SIGHUP]:
            with VM('control-signal-' + sig.name, ['--vcpus', '4', '--control-socket', str(sock)], binary=binary) as vm:
                vm.ready()
                request('pause')
                vm.p.send_signal(sig)
                vm.wait_exit()
            assert not sock.exists()
    print('PASS: 1/4 vCPU pause/resume, offline CPU, queued UART, concurrent clients, stop response, paused signals, deferred Ctrl-], poweroff, startup failure, early return, concurrent signal/socket stop, memory target, terminal/socket cleanup')


if __name__ == '__main__':
    main()
