#!/usr/bin/env python3
"""Real guest network tests; use --peer for the entirely local NetDevice test peer."""
import argparse
import os
import json
import socket
import time
import hashlib
import http.server
import ipaddress
import pathlib
import subprocess
import tempfile
import threading
from smoke import LOGS, ROOT, VM

ap = argparse.ArgumentParser()
ap.add_argument("--binary", default=str(ROOT / "dist/w-vmm"))
ap.add_argument("--memory", action="store_true", help="exercise 4 CPUs and concurrent virtio-mem resizing with --peer")
ap.add_argument("--peer", action="store_true", help="binary is the signed net-peer example; requires no vmnet privileges")
ap.add_argument("--guest-cidr", help="unused guest static IPv4/prefix on the vmnet subnet")
ap.add_argument("--host-ip", help="host IPv4 address on the vmnet subnet")
args = ap.parse_args()
if args.memory and not args.peer:
    ap.error("--memory requires --peer")
if args.peer:
    guest = ipaddress.IPv4Interface("192.0.2.2/24")
    host = ipaddress.IPv4Address("192.0.2.1")
else:
    if not args.guest_cidr or not args.host_ip:
        ap.error("vmnet testing requires --guest-cidr and --host-ip; address allocation is explicit")
    guest = ipaddress.IPv4Interface(args.guest_cidr)
    host = ipaddress.IPv4Address(args.host_ip)
    if host not in guest.network or host == guest.ip:
        ap.error("host and guest must use distinct addresses on the same subnet")
binary = str(pathlib.Path(args.binary).resolve())
LOGS.mkdir(exist_ok=True)

with tempfile.TemporaryDirectory(prefix="w-vmm-net-") as temp:
    if args.memory:
        os.environ["W_VMM_VCPUS"] = "4"
        os.environ["W_VMM_MEMORY_SOCKET"] = str(pathlib.Path(temp) / "control.sock")
    disks = []
    for i in range(2):
        disk = pathlib.Path(temp) / f"disk{i}.qcow2"
        subprocess.run([str(ROOT / "scripts/create-disk.sh"), str(disk)], check=True)
        disks.append(str(disk))
    # The local peer covers both network-only and network-with-two-disks attachment.
    for attached in [[], disks]:
        opts = attached if args.peer else ["--net", *sum((["--disk", p] for p in attached), [])]
        with VM("net-" + ("peer" if args.peer else "vmnet") + f"-{len(attached)}-disks", opts, binary=binary) as vm:
            vm.ready()
            resize_stop = threading.Event()
            resize_errors, resized = [], []
            def request(command):
                with socket.socket(socket.AF_UNIX) as conn:
                    conn.settimeout(2)
                    conn.connect(os.environ["W_VMM_MEMORY_SOCKET"])
                    conn.sendall(json.dumps(command).encode() + b"\n")
                    response = json.loads(conn.makefile("rb").readline())
                    assert response["ok"], response
                    return response["status"]
            def resize_loop():
                try:
                    while not resize_stop.is_set():
                        for target in [0, 384, 128]:
                            request({"command": "memory-set", "requested_mib": target})
                            deadline = time.monotonic() + 30
                            while request({"command": "memory-status"})["plugged_size_mib"] != target:
                                if time.monotonic() > deadline: raise TimeoutError("memory resize under network load")
                                time.sleep(0.01)
                            resized.append(target)
                except Exception as error:
                    resize_errors.append(error)
            resize_thread = threading.Thread(target=resize_loop, daemon=True) if args.memory else None
            if resize_thread: resize_thread.start()
            vm.command("test -e /sys/class/net/eth0 && test -z \"$(ip -4 addr show dev eth0 | grep 'inet ')\" && ! pidof udhcpc", "UNCONFIGURED_PASS")
            if attached:
                vm.command('test "$(cat /sys/block/vda/serial)" = disk0 && test "$(cat /sys/block/vdb/serial)" = disk1', "NET_DISKS_PASS")
                vm.command("mkdir -p /data2 && mount /dev/vda /data && mount /dev/vdb /data2 && echo first > /data/identity && echo second > /data2/identity && sync && umount /data && umount /data2", "NET_DISK_IO_PASS")
            vm.command(f"ip link set eth0 up && ip addr add {guest} dev eth0", "STATIC_IP_PASS")
            if args.memory:
                vm.send(f'ping -i 0.01 -s 1472 {host} > /tmp/pause-ping & pingpid=$!')
                for _ in range(10):
                    assert request({"command": "pause"})["lifecycle"] == "Paused"
                    time.sleep(0.02)
                    assert request({"command": "resume"})["lifecycle"] == "Running"
                vm.command('kill -INT "$pingpid"; wait "$pingpid"; true', 'PAUSED_PING_OK')
            # Exercise full-size frames and enough packets to wrap a 128-entry ring.
            vm.command(f"ping -c 140 -i 0.01 -s 1472 -W 2 {host} > /tmp/ping && cat /tmp/ping && grep -q '140 packets received, 0% packet loss' /tmp/ping", "PING_PASS")
            if not args.peer:
                payload = bytes(range(256)) * 1024
                digest = hashlib.sha256(payload).hexdigest()
                uploaded = threading.Event()

                class Handler(http.server.BaseHTTPRequestHandler):
                    def do_GET(self):
                        self.send_response(200)
                        self.send_header("Content-Length", str(len(payload)))
                        self.end_headers()
                        self.wfile.write(payload)

                    def do_POST(self):
                        length = int(self.headers.get("Content-Length", "0"))
                        valid = length == len(payload) and self.rfile.read(length) == payload
                        if valid:
                            uploaded.set()
                        self.send_response(200 if valid else 400)
                        self.send_header("Content-Length", "0")
                        self.end_headers()

                    def log_message(self, *_):
                        pass

                with http.server.HTTPServer((str(host), 0), Handler) as server:
                    worker = threading.Thread(target=server.serve_forever, daemon=True)
                    worker.start()
                    try:
                        url = f"http://{host}:{server.server_port}/payload"
                        vm.command(f"wget -q -O /tmp/payload {url} && echo '{digest}  /tmp/payload' | sha256sum -c -", "DOWNLOAD_PASS")
                        vm.command(f"wget -q -O /dev/null --post-file=/tmp/payload {url}", "UPLOAD_PASS")
                        assert uploaded.wait(5), "host did not receive the matching payload"
                    finally:
                        server.shutdown()
                        worker.join()
            if resize_thread:
                resize_stop.set()
                resize_thread.join(35)
                assert not resize_thread.is_alive()
                assert not resize_errors, resize_errors
                assert len(resized) >= 3, resized
                print("Concurrent memory resize targets:", resized)
            vm.poweroff()
    for disk in disks:
        subprocess.run(["qemu-img", "check", disk], check=True)
print("PASS: guest network discovery, explicit static IP, ARP/ICMP, queue wrap, large frames, network with multiple disks" + ("" if args.peer else ", TCP upload/download"))
