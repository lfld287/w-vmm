# w-vmm

在 Apple Silicon 上通过 macOS Hypervisor.framework 直接启动 Alpine ARM64 的独立 Rust VMM。1 个 vCPU，默认 512 MiB RAM，原生 GICv3，16550 串口 shell，多块可命名 qcow2 数据盘及可选 virtio-net 网卡。内核和 initramfs 编译进可执行文件；客户机根文件系统驻留内存。

所有准备、构建、运行和验收均在本机完成。没有上传、远程托管或部署步骤。

## 直接运行

执行根目录的构建脚本即可生成完整的 `dist/` 运行包：

```sh
./build.sh
cd dist
./run.sh
# 或：./w-vmm run --disk data.qcow2 --read-only
```

看到 `W-VMM ALPINE READY` 和 `w-vmm:~#` 后可直接操作 shell：

```sh
uname -a
mount /dev/vda /data
echo hello > /data/hello.txt
sync
umount /data
poweroff
```

只读盘使用 `mount -o ro /dev/vda /data`。程序不会自动格式化或自动挂载数据盘。不传 `--disk` 时没有 `/dev/vda`。RAM 支持 128–16384 MiB；默认 512 MiB。

`poweroff` 正常关机；`reboot` 使宿主进程退出，需要重新执行命令启动。宿主 `Ctrl-]` 可退出，`Ctrl-C` 传给客户机 shell。SIGINT、SIGTERM、SIGHUP 会使宿主停止 vCPU、刷新磁盘并恢复终端。强制退出前应在客户机执行 `sync` / `umount`；宿主只能刷新已经收到的磁盘请求，不能代替客户机刷新文件系统。SIGKILL 或宿主崩溃无法执行清理。

## 从源码准备与构建

需要 Apple Silicon、macOS 15+、Xcode Command Line Tools、Rust（本机验证版本 1.98.0）和 Python 3。磁盘准备/验收额外需要 `qemu-img` 和 `e2fsprogs`，运行 VMM 不需要它们。

```sh
./build.sh
```

准备脚本下载固定 SHA-256 的 Alpine 3.24.1 aarch64 minirootfs 和 `linux-virt-6.18.52-r0.apk`。内核与模块来自同一个包。脚本解析 EFI zboot gzip 封装生成 ARM64 Image，选择 virtio-mmio、virtio-blk、virtio-net、ext4 及模块依赖，在 Mac 上生成确定性 newc/gzip initramfs。BusyBox init 作为 PID 1 回收子进程、启动串口 shell 并负责关机。

下载缓存位于 `.cache/`，嵌入文件位于 `assets/Image` 和 `assets/initramfs.cpio.gz`，来源与校验值见 `assets/manifest.json`。普通 Cargo 构建不下载启动资源，缺少资源会直接编译失败。缓存存在后可离线重新打包；依赖已缓存时可执行 `cargo build --release --locked --offline`。

`build.sh` 在缺少启动资源时自动执行准备脚本，然后构建并将程序、启动脚本、示例盘、资源清单和许可说明整理到 `dist/`。整个 `dist/` 可以移动到其他目录直接运行；无需旁边存在源码、内核或 initramfs。首次创建示例盘需要 `qemu-img` 和 `e2fsprogs`，后续构建保留已有 `dist/data.qcow2`。Cargo 中间产物仍位于 `target/`，下载缓存仍位于 `.cache/`。`scripts/build.sh` 保留为兼容入口。

构建脚本设置 `MACOSX_DEPLOYMENT_TARGET=15.0`，并为 `dist/w-vmm` 附加本地签名：

```sh
codesign --force --sign - --entitlements assets/entitlements.plist dist/w-vmm
```

每次重新构建可执行文件后都需要重新签名。直接 `cargo run -p w-vmm-demo --bin w-vmm -- run` 生成的程序可能缺少 entitlement。

## 作为库使用

根包 `w-vmm` 仅提供 library；`demo/` 中的 `w-vmm-demo` 通过路径依赖调用库，并提供名为 `w-vmm` 的 CLI。两者组成 workspace，共用根目录的 `Cargo.lock` 和 `target/`。默认 `cargo build` 构建库和 demo；仅构建库可执行 `cargo build -p w-vmm --lib --locked`。

在本地调用项目的 `Cargo.toml` 中添加（路径替换为本仓库的位置）：

```toml
[dependencies]
w-vmm = { path = "../w-vmm" }
```

```rust
use std::{collections::BTreeMap, io::{self, Write}};
use w_vmm::{VmConfig, Vmm, storage::Disk, net::macos::Vmnet, serial::SerialIo};

// Output only; the guest can stop the VM with poweroff.
struct Console;
impl Write for Console {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        io::stdout().write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}
impl SerialIo for Console {
    fn recv(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    Vmm::new(VmConfig::default()).run::<Disk, Vmnet, _>(BTreeMap::new(), None, Console)?;
    Ok(())
}
```

`VmConfig` 仅配置 `memory_mib`。后端由调用方创建并交给泛型入口：

```rust,ignore
pub fn run<BS: BlockStorage, ND: NetDevice, SI: SerialIo>(
    self,
    blocks: BTreeMap<String, BS>,
    net: Option<ND>,
    serial: SI,
) -> anyhow::Result<()>;
```

多个磁盘使用 `BTreeMap<String, BS>`，空 map 表示无磁盘；网络使用 `Option<ND>`。例如：

```rust,no_run
use std::{collections::BTreeMap, path::Path};
use w_vmm::{VmConfig, Vmm, storage::Disk, net::macos::Vmnet, serial::SerialIo};

fn run(serial: impl SerialIo) -> Result<(), Box<dyn std::error::Error>> {
    let blocks = BTreeMap::from([
        ("sda".into(), Disk::open(Path::new("system.qcow2"), true)?),
        ("sdb".into(), Disk::open(Path::new("data.qcow2"), false)?),
    ]);
    Vmm::new(VmConfig::default()).run(blocks, None::<Vmnet>, serial)?;
    Ok(())
}
```

磁盘名允许 1–20 个 ASCII 字母、数字、`.`、`_`、`-`，按名称排序分配设备地址，名称通过 virtio-blk GET_ID 暴露为 serial。客户机使用 `/sys/block/vda/serial` 等识别磁盘；`sda` 是注册名，不会把 virtio 磁盘改名为 `/dev/sda`。Linux 分配 `/dev/vda`、`/dev/vdb` 等名称，添加/删除排序靠前的设备可能改变这些名称。

同一 map 可用 `Box<dyn BlockStorage>` 混合不同存储实现；已提供 `Box<T>` 的 trait 转发。`BlockStorage` 本身保持原有接口。`Vmm` 接管后端所有权，退出时尝试刷新全部磁盘；即使一块盘刷新失败也继续处理其余磁盘。

`net::NetDevice` 提供 `mac_address`、`mtu`、`max_frame_len` 和非阻塞 `send` / `recv`，传输完整以太网帧，不含 FCS 或 virtio 头。`send` 返回 `false` 表示未消费该帧、稍后重试；`recv` 返回 `None` 表示没有数据。trait 不涉及 DHCP、IP、路由或 NAT；这些服务由具体后端决定。实现不需要 `Send` / `Sync`，也可以使用 `Box<dyn NetDevice>`。

`serial::SerialIo: std::io::Write` 是必传的串口后端。`recv(&mut [u8])` 非阻塞读取发往客户机的数据，返回长度不得超过缓冲区；`0`（含 EOF）、`WouldBlock` 和 `Interrupted` 表示暂无输入。每轮只读取 UART FIFO 剩余容量，满时不读取。`should_stop()` 默认返回 `false`，每轮独立检查，FIFO 满时也能停止。所有调用在 VMM 线程执行，无需 `Send` / `Sync`，支持 `Box<dyn SerialIo>`。

客户机输出使用同步 `write_all` / `flush`，没有额外输出队列。输入的其他错误及输出错误会终止运行，并尝试刷新全部磁盘。库不解释 `0x1d` 等控制字节；终端 raw mode、stdin/stdout、Ctrl-]、信号和恢复由 `demo/src/terminal.rs` 的 `Terminal` 实现负责。自定义后端需要退出时通过 `should_stop()` 表达。

内核和 initramfs 仍由库提供；调用程序需附加相同的 Hypervisor entitlement 本地签名。`run` 已改为必须传入三个后端参数，不再隐式创建终端。

## 多盘和网络

```sh
./dist/w-vmm run --disk sda=system.qcow2 --disk sdb=data.qcow2
./dist/w-vmm run --disk sda=system.qcow2 --disk sdb=data.qcow2 --read-only
```

`--disk` 可重复；未指定名称的路径依次命名为 `disk0`、`disk1` 等。CLI 拒绝重复名称，`--read-only` 作用于所有指定的磁盘；库调用可以分别设置各盘只读状态。

macOS 的 `net::macos::Vmnet::shared()` 使用系统 vmnet.framework 提供 NAT。默认没有网卡，CLI 通过 `--net` 启用：

```sh
sudo ./dist/w-vmm run --net --disk sda=data.qcow2
```

vmnet 需要 root 或 Apple 批准的 `com.apple.vm.networking` entitlement。现有 ad-hoc Hypervisor 签名不提供这一权限；构建脚本不会自动提权或添加受限 entitlement。运行时不需要额外网络库、守护进程或外部工具。

客户机仅加载网卡驱动，不自动配置 IP、路由或启动 DHCP 客户端。`Vmnet::ipv4()` 提供具体后端返回的网关、地址池上界及子网掩码，demo 在启动时打印这些信息。**每次测试都先核对启动输出，不能固定假设网关是 `192.168.64.1` 或其他示例地址。** 手动配置和测试步骤见下方“vmnet 手动测试”。

首版为单 RX/TX 队列对、MTU 1500 的 vmnet 后端；virtio-net 不提供 checksum/GSO offload、合并接收缓冲、控制队列或多队列。

## 创建示例数据盘

```sh
brew install qemu e2fsprogs
./scripts/create-disk.sh extra-data.qcow2 256M
```

脚本拒绝覆盖已有文件，创建整个磁盘为 ext4 的镜像，然后转换成 64 KiB cluster 的 qcow2 v3。`MKE2FS` 可指定 e2fsprogs 的程序路径。QEMU 仅作为镜像工具使用，不参与客户机运行。

支持独立、未加密的 qcow2 v2/v3；拒绝 backing chain、外部数据文件、内部快照和不支持的 incompatible feature（包括 dirty/corrupt 标记）。无快照操作、discard 或压缩 cluster 写入。写入打开持有排他 advisory flock；只读打开持有共享锁。外部工具也应遵守锁，不能在 VM 运行时修改镜像。

## 验证

```sh
cargo build --workspace --locked
cargo check --workspace --all-targets --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p w-vmm --lib --locked
./build.sh
python3 scripts/smoke.py --binary dist/w-vmm
```

单元测试覆盖串口双向字节、FIFO 容量、空输入、停止请求、读写/刷新错误、控制字节直通和 trait 对象，以及设备树与多设备地址分配、公共 MMIO 协商/复位/双队列/中断、描述符校验、多盘状态隔离、磁盘 I/O 错误、网络分散缓冲收发、背压重试、短缓冲丢包和后端错误。

冒烟脚本仅创建临时测试盘，日志保存在 `test-results/`。执行真实 HVF 启动、shell 命令、多盘 serial 映射与独立持久化、ext4 挂载、192 KiB 随机数据写入与 SHA-256 校验、同步/卸载/关机/重启读回、只读镜像哈希不变、重复打开锁冲突、损坏文件、宿主文件大小限制导致的真实 EFBIG、重启退出和信号/快捷键后的终端恢复。关机后调用 `qemu-img check`。

离线测试把可执行文件和数据盘放入仅含这两个文件的临时目录启动。未指定网络后端时不会创建宿主网络接口；运行时不会调用外部工具。`otool -L` 的链接项包括 Hypervisor.framework、vmnet.framework 和 macOS 系统库。验收宿主为 macOS 27.0 / Apple Silicon，尚未在 macOS 15 真机上回归。

### vmnet 手动测试

先在 **Mac 终端、仓库根目录** 构建并启动；测试网络无需挂载磁盘：

```sh
./build.sh
sudo ./dist/w-vmm run --net
```

检查启动日志中的 `IPv4 SharedIpv4`，例如本机一次实际测试返回：

```text
IPv4 SharedIpv4 { gateway: Some("192.168.2.1"), pool_end: Some("192.168.2.254"), subnet_mask: Some("255.255.255.0") }
```

这代表网关 `192.168.2.1`、子网 `192.168.2.0/24`。以下命令**仅适用于这组输出**；若网关或掩码不同，先替换地址和前缀长度。`192.168.2.10` 是手工选择的临时客户机地址，需要确认未被其他设备占用，并非 vmnet 分配的固定地址。

进入 **客户机 `w-vmm:~#` shell** 后，清除先前的错误配置并测试网关：

```sh
ip -4 addr flush dev eth0
ip link set eth0 up
ip addr add 192.168.2.10/24 dev eth0
ip route replace default via 192.168.2.1 dev eth0
ping -c 3 192.168.2.1
```

网关能回复后，再验证路由和 NAT 出网；直接访问公网 IP，不涉及 DNS：

```sh
ip route get 1.1.1.1
ping -c 3 1.1.1.1
```

`ip route get` 仅显示选路结果，不代表网关可达。若网关不通，先核对启动日志与 `ip -4 addr show dev eth0`，再查看 `ip neigh show dev eth0`；`INCOMPLETE` / `FAILED` 表示邻居解析尚未成功，需要检查地址和二层收发。如果网关可达而公网测试失败，再检查宿主出网与 NAT。

手动测试结束后在客户机执行 `poweroff`，释放地址，再运行下面的自动化测试。

### 本地测试后端

无需 vmnet 权限的真实客户机网络测试使用 `demo/examples/net-peer.rs`，它通过公开的 `NetDevice` 注入一个只响应 ARP/ICMP 的本地测试对端，并复用 demo 的 `Terminal`：

```sh
cargo build -p w-vmm-demo --example net-peer --locked
codesign --force --sign - --entitlements assets/entitlements.plist target/debug/examples/net-peer
python3 scripts/smoke-net.py --peer --binary target/debug/examples/net-peer
```

测试覆盖网卡发现、启动后未配置 IP、显式静态地址、整 MTU 大包、队列环回及网络与两块磁盘共同工作。它不连接宿主网络，不验证 vmnet 的 NAT 服务。

### vmnet 自动化测试

在 **Mac 终端、仓库根目录** 执行。需要 Python 3、`qemu-img`、`e2fsprogs` 和 vmnet 运行权限。先按手动测试确认实际子网，并停止使用同一测试地址的虚拟机。下例沿用上面的实际网关与已确认空闲的客户机地址，其他子网必须替换参数：

```sh
sudo env "PATH=$PATH" python3 scripts/smoke-net.py \
  --binary dist/w-vmm \
  --guest-cidr 192.168.2.10/24 \
  --host-ip 192.168.2.1
```

保留 `PATH` 使提权后的脚本能够找到 Homebrew 安装的磁盘工具。`--host-ip` 必须是宿主在该 vmnet 子网中的实际地址；本次测试与启动日志的网关一致。

脚本只创建临时测试盘，依次验证无磁盘/两块磁盘与网络共存、启动时未配置 IP、显式静态地址、140 次整 MTU ping，以及 256 KiB TCP 下载、SHA-256 校验和上传。成功后输出 `PASS`；日志位于 `test-results/net-vmnet-0-disks.log` 和 `test-results/net-vmnet-2-disks.log`。该脚本只测试客户机与宿主通信；公网 NAT 使用前面的手动步骤验证。

当前已通过普通测试、真实多盘冒烟及本地测试后端网络冒烟；按实际子网配置后的手动 vmnet 连通性也已确认通过。真实 vmnet 的自动化 TCP 上传下载尚未完成验收。

## 代码结构

- `Cargo.toml`：根 library 包和 workspace 入口，共用 `Cargo.lock`、`target/` 及 release profile。
- `demo/Cargo.toml`：不可发布的 `w-vmm-demo` 包，声明 `w-vmm` 二进制及 CLI 依赖。
- `src/boot.rs`：校验 ARM64 Image、RAM/内核/initramfs/FDT 布局，linux-loader 加载及 vm-fdt 设备树。
- `src/platform/mod.rs`：`VmRuntime` trait 与按构建目标的运行时选择（`Runtime` 别名、`run()` 分派）。
- `src/platform/macos_arm64/mod.rs`：`VmRuntime` 的 macOS/arm64 实现——HVF 运行循环、vCPU 唤醒（Kicker）与串口中断接线；`#[cfg]` 门控，仅 Apple Silicon 编译。
- `src/platform/macos_arm64/hvf.rs`：最小 FFI、VM 映射、原生 GIC 和绑定创建线程的 RAII vCPU。VM 内存由 vm-memory 持有，先销毁 vCPU 和映射，再释放 RAM。
- `src/devices/mmio.rs`：共享 modern virtio-mmio、功能协商、队列配置、描述符校验、复位及中断确认。
- `src/devices/block.rs`：磁盘请求与配置，单个 128 项 split virtqueue，IN/OUT/FLUSH/GET_ID，单请求上限 1 MiB。
- `src/devices/net.rs`：网络请求与配置，RX/TX 各 128 项队列，完整帧收发与发送背压。
- `src/net.rs`、`src/net/macos.rs`：`NetDevice` trait 与 macOS vmnet NAT 后端。
- `src/storage.rs`：imago 原生同步 qcow2、同 inode 文件锁、范围校验、内部缓存 flush 与宿主 sync/fsync。
- `src/serial.rs`：`SerialIo` trait、Box 转发和 FIFO 输入轮询。
- `demo/src/terminal.rs`：原始终端、非阻塞输入、Ctrl-] 与信号恢复，CLI 和网络示例共用。
- `src/lib.rs`：`VmConfig` / `Vmm::new(...).run(blocks, net, serial)`，委托 `platform::run`。
- `demo/src/main.rs`：CLI、错误输出，通过路径依赖调用根库。

设备 MMIO：GIC distributor `0x08000000`，16550 `0x09000000`（SPI 33），virtio 设备从 `0x0a000000` 开始，每个占 `0x1000`，中断从 SPI 34 递增。先按名称排列磁盘，再放置可选网卡，每个设备使用独立 MMIO 和中断。GIC redistributor 位于 `0x10000000`，RAM 位于 `0x40000000`。实际可用 SPI 范围、定时器 INTID 和 redistributor 空间大小查询 HVF；设备超出范围时启动报错。

初版无图形、SMP、快照、热插拔或其他宿主平台。轮询唤醒周期为 10 ms，网络每轮每方向最多处理 64 个包，存储请求同步执行，未做吞吐/延迟优化。非法设备访问/队列结构会报错并退出；普通磁盘请求 I/O 错误返回 virtio IOERR 并记录原因。此实现尚未经过不可信客户机的安全审计。

第三方来源与许可见 [THIRD_PARTY.md](THIRD_PARTY.md)。
