# w-vmm

在 Apple Silicon 上通过 macOS Hypervisor.framework 直接启动 Alpine ARM64 的独立 Rust VMM。可配置并行 vCPU，默认 1 核、512 MiB 基础 RAM，可选 virtio-mem 动态内存，原生 GICv3，16550 串口 shell，多块可命名 qcow2 数据盘及可选 virtio-net 网卡。内核和 initramfs 编译进可执行文件；客户机根文件系统驻留内存。

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

只读盘使用 `mount -o ro /dev/vda /data`。程序不会自动格式化或自动挂载数据盘。不传 `--disk` 时没有 `/dev/vda`。基础 RAM 最低 128 MiB，默认 512 MiB；无固定容量上限，受 HVF IPA 地址范围和主机资源限制。

默认以 strict 模式运行：客户机违反 virtio 或设备协议即停止整个 VM，便于尽早暴露问题。`./w-vmm run --lenient ...` 改为把客户机错误限制在出错设备内，详见下文“客户机错误处理”。

`poweroff` 正常关机；`reboot` 使宿主进程退出，需要重新执行命令启动。宿主 `Ctrl-]` 可退出，`Ctrl-C` 传给客户机 shell。SIGINT、SIGTERM、SIGHUP 会使宿主停止 vCPU、刷新磁盘并恢复终端。强制退出前应在客户机执行 `sync` / `umount`；宿主只能刷新已经收到的磁盘请求，不能代替客户机刷新文件系统。SIGKILL 或宿主崩溃无法执行清理。

## 从源码准备与构建

需要 Apple Silicon、macOS 15+、Xcode Command Line Tools、Rust（本机验证版本 1.98.0）和 Python 3。磁盘准备/验收额外需要 `qemu-img` 和 `e2fsprogs`，运行 VMM 不需要它们。

```sh
./build.sh
```

准备脚本下载固定 SHA-256 的 Alpine 3.24.1 aarch64 minirootfs 和 `linux-virt-6.18.52-r0.apk`。内核与模块来自同一个包。脚本解析 EFI zboot gzip 封装生成 ARM64 Image，选择 virtio-mmio、virtio-blk、virtio-net、virtio-mem、ext4 及模块依赖，在 Mac 上生成确定性 newc/gzip initramfs。BusyBox init 作为 PID 1 回收子进程、启动串口 shell 并负责关机。

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
    Vmm::new(VmConfig::default(), BTreeMap::<String, Disk>::new(), None::<Vmnet>, None, Console).run()?;
    Ok(())
}
```

`VmConfig` 配置 `memory_mib`、`vcpu_count` 和 `strict`（默认 `true`，见“客户机错误处理”）。后端由调用方创建并交给泛型入口：

```rust,ignore
impl<BS: BlockStorage, ND: NetDevice, SI: SerialIo> Vmm<BS, ND, SI> {
    pub fn run(self) -> w_vmm::Result<()>;
    pub fn run_with_platform<P: Platform>(self, platform: P) -> w_vmm::Result<()>;
}
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
    Vmm::new(VmConfig::default(), blocks, None::<Vmnet>, None, serial).run()?;
    Ok(())
}
```

磁盘名允许 1–20 个 ASCII 字母、数字、`.`、`_`、`-`，按名称排序分配设备地址，名称通过 virtio-blk GET_ID 暴露为 serial。客户机使用 `/sys/block/vda/serial` 等识别磁盘；`sda` 是注册名，不会把 virtio 磁盘改名为 `/dev/sda`。Linux 分配 `/dev/vda`、`/dev/vdb` 等名称，添加/删除排序靠前的设备可能改变这些名称。

同一 map 可用 `Box<dyn BlockStorage>` 混合不同存储实现；已提供 `Box<T>` 的 trait 转发。`BlockStorage` 的可失败方法返回 `Result<T, StorageError>`。`Vmm` 接管后端所有权，退出时尝试刷新全部磁盘；即使一块盘刷新失败也继续处理其余磁盘。

`net::NetDevice` 提供 `mac_address`、必填关联常量 `MTU: u16`、`max_frame_len` 和非阻塞 `send` / `recv`，传输完整以太网帧，不含 FCS 或 virtio 头。`send` 返回 `false` 表示未消费该帧、稍后重试；`recv` 返回 `None` 表示没有数据。trait 不涉及 DHCP、IP、路由或 NAT；这些服务由具体后端决定。实现不需要 `Send` / `Sync`，可用 `Box<T>` 包装具体后端，关联常量会转发给 `T`。

`BlockStorage::read/write` 和 `NetDevice::send/recv` 的数据参数统一为 `&[vm_memory::VolatileSlice<'_>]`，长度是各段字节数之和。后端必须在调用返回前完成内存访问，不得保存 guest 指针或自行把 guest 数据转换为普通 Rust slice；使用 imago 官方转换器时，其内部引用处理由 imago 管理。各后端自行选择复制策略，可以通过 volatile 复制访问数据，或持有 `ptr_guard` / `ptr_guard_mut`，在同步系统调用中传递原始 `iovec`。`recv(None)` 不得修改接收缓冲；磁盘读失败允许部分目标数据已被修改，但设备只报告 IOERR 和状态字长度。

普通 vmnet TX 直接发送 guest 分段；背压时复制一次到可复用宿主快照，后续重试使用该快照，reset 丢弃它。RX 在容量达到最大帧长且分段不重叠时直接写入 guest；无包会撤销出队，小缓冲和重叠区间使用可复用回退缓冲。设备层将 guest 分段直接传递给 `NetDevice` / `BlockStorage`，这里的“零拷贝”限定于自定义接口边界，不保证具体后端内部零拷贝。

`Disk` 固定使用官方 imago `=0.2.5`，启用 `sync` 和 `vm-memory` features，不引入 vendor 或补丁。通过官方 `IoVectorMut::from_volatile_slice` / `IoVector::from_volatile_slice` 转换器将 guest 分段缓冲直接传给同步 `readv` / `writev`，guard 保持到调用结束；内部缓冲和引用处理由 imago 管理。普通数据路径省去请求级用户态 payload 中转复制；压缩簇、对齐回退和内核数据搬运不属于零拷贝承诺。

**已知限制（共享内存别名）：** imago 0.2.5 在内部把 guest `VolatileSlice` 转换为普通 Rust slice（`io_buffers.rs` 的 `from_raw_parts(_mut)`，零填充路径的 `fill`）。这些页属于 guest 共享内存，其他 vCPU 可能同时访问，按 Rust 别名模型属于未定义行为。为保留零拷贝，目前有意接受这一限制，等待 imago 上游修复；自定义 `BlockStorage` 后端不应仿照这种做法。

完整请求的长度溢出及容量范围在任何 I/O 前校验，忽略空段，按宿主 `IOV_MAX` 分批（查询失败使用 16）；读取目标重叠时先完成当前批次，再按原顺序推进磁盘偏移。读取失败可能已修改部分目标，设备仍报告 IOERR。只读检查、空请求和刷新语义保持不变。

`serial::SerialIo: std::io::Write` 是必传的串口后端。`recv(&mut [u8])` 非阻塞读取发往客户机的数据，返回长度不得超过缓冲区；`0`（含 EOF）、`WouldBlock` 和 `Interrupted` 表示暂无输入。每轮只读取 UART FIFO 剩余容量，满时不读取。暂停期间不调用 `recv()`，输入留在宿主缓冲区，恢复后继续按 FIFO 容量读取。所有调用在 VMM 线程执行，无需 `Send` / `Sync`，支持 `Box<dyn SerialIo>`。

客户机输出使用同步 `write_all` / `flush`，没有额外输出队列。输入的其他错误及输出错误会终止运行，并尝试刷新全部磁盘。库不解释 `0x1d` 等控制字节；终端 raw mode、stdin/stdout、Ctrl-]、信号和恢复由 `demo/src/terminal.rs` 的 `Terminal` 实现负责。`Terminal::new()` 返回 `(Terminal, TerminalGuard)`，调用方必须保留 guard 直到 `run()` 返回。信号和 Ctrl-] 通过本地通知通道唤醒独立线程，再调用同步 `VmControl::stop()`；串口销毁只恢复终端，guard 注销信号并回收线程。自定义后端需要退出时也应通知其他线程调用 `VmControl::stop()`，不可在 VMM 回调内调用阻塞控制方法。

内核和 initramfs 仍由库提供；调用程序需附加相同的 Hypervisor entitlement 本地签名。磁盘、网络、内存设备和串口在构造时传入，`run()` 不再接收设备参数，也不隐式创建终端。

## 错误类型（公开 API 变更）

工作区使用 `thiserror = "2"`。根模块导出 `Error` 和 `Result<T>`（即 `std::result::Result<T, w_vmm::Error>`）；`error` 模块按业务边界定义错误，顶层 `Error` 提供各模块的 `From` 转换。

| 接口 | 错误类型（位于 `w_vmm::error`） |
| --- | --- |
| `BlockStorage`、`Disk`、存储校验 | `StorageError` |
| `NetDevice`、`Vmnet` | `NetError` |
| `Mapper`、`VirtioMem` | `MemoryError` |
| `Platform`、`VirtualMachine` | `PlatformError` |
| `boot` 辅助函数 | `BootError` |
| `VmControl` 可失败方法 | `ControlError` |
| `Vmm::run`、`run_with_platform` | `Error` |

`SerialIo` 和标准读写接口仍返回 `std::io::Result`，内部 UART 边界转换为 `SerialError`。设备协议与运行时错误分别为 `DeviceError`、`RuntimeError`。demo 独立定义参数、终端和控制协议错误。

自定义后端需更新 trait 方法的返回类型；可以通过 `Backend` 包装自己的错误：

```rust
use w_vmm::error::StorageError;

fn flush_backend(file: &std::fs::File) -> Result<(), StorageError> {
    file.sync_all().map_err(|source| StorageError::Backend(Box::new(source)))
}
```

`StorageError`、`NetError`、`MemoryError`、`PlatformError` 的 `Backend` 接受 `Box<dyn std::error::Error + Send + Sync + 'static>`；这些约束只作用于错误，后端对象仍无需 `Send` / `Sync`。外部项目可选用 `thiserror 2` 定义自己的错误。

错误变体保留容量、地址、请求长度、状态码等字段，I/O 上下文保留操作、路径及原始 `source`。可以匹配具体变体，使用标准 `Error::source()` 遍历原因，或用 `w_vmm::error::diagnostic(&error)` 输出完整诊断。控制线程回复和 `VmStatus::final_error` 仍为字符串；回复失败与重复 `stop()` 的清理失败分别返回 `ControlError::ReplyFailed`、`ControlError::CleanupFailed`，保留相同诊断。

## 客户机错误处理（strict / lenient）

`VmConfig::strict` 默认为 `true`，适合测试和 fuzz：任何客户机协议错误都会让 `run()` 返回错误。设为 `false`（demo 为 `--lenient`）时，客户机错误只影响出错的设备：

```rust,ignore
let config = VmConfig { strict: false, ..VmConfig::default() };
```

| 情形 | strict（默认） | lenient |
| --- | --- | --- |
| 非法寄存器写入（清除状态位、队列未就绪时 DRIVER_OK、非法 QueueReady/QueueNum、DRIVER_OK 后重新配置或启用队列） | `DeviceError` 退出 | 忽略本次写入且不产生任何副作用，按 2 的幂次限流记录日志 |
| 队列或描述符损坏（环溢出、越界、环路、间接描述符、不可信的块状态描述符、非法网络帧方向或长度、virtio-mem 请求缓冲不合法） | `DeviceError` 退出 | 仅该设备置 `DEVICE_NEEDS_RESET (0x40)` 并停止其数据面；已设置 DRIVER_OK 时发送配置变更中断（InterruptStatus bit 1）。该状态只能由设备复位（写状态 0）清除 |
| 设备窗口内无法解码的访问（宽度非 1/2/4/8、virtio 寄存器非 32 位访问、UART 非单字节访问、配置空间越过窗口） | `RuntimeError::InvalidAccess` 退出 | 读返回对应宽度的全 1，写忽略 |
| 访问起点不在任何已声明窗口内 | `RuntimeError::UnmappedIo` 退出 | 同左 |
| 块请求格式错误或后端 I/O 失败（状态描述符可信） | 完成请求并返回 IOERR，未知操作返回 UNSUPP | 同左 |
| 宿主错误：后端失败（含复位时的磁盘刷新）、后端违反接口约定（如网络帧长度越界）、HVF 调用失败 | 退出 | 同左 |
| virtio-mem 映射失败 | 完整回滚成功时向客户机返回失败响应；回滚失败退出 | 同左 |

两种模式共同的规则：

- 状态写入先校验再提交。驱动选择的特性不受支持时，FEATURES_OK 写入会被清除（规范规定的拒绝方式）；同一次写入若还设置了 DRIVER_OK，整次写入被拒绝，状态保持不变。QueueReady=1 校验失败时恢复为未就绪。
- DRIVER_OK 之后允许写 QueueReady=0 停用队列，但不支持再次启用（需要设备复位）。停用的队列不再接受通知、轮询和动态内存 pin 扫描，网络设备挂起的 TX 帧也不再重试，直到复位。
- 动态内存事务逐个设备扫描待处理描述符：lenient 下出错的设备被隔离，其队列不再参与 pin；virtio-mem 自身被隔离时跳过本次请求处理并恢复 vCPU。
- 设备状态变化后先更新中断电平，再完成 vCPU 的 I/O（包括 InterruptACK 和读取 UART IIR），vCPU 不会看到过期的中断线。

### 迁移

- `VmConfig` 新增公开字段 `strict`，默认 `true`。结构体字面量需要补上该字段或 `..VmConfig::default()`。默认值沿用 fail-stop，但 strict 比以前更严格，见下文“行为变化”。
- 删除的错误变体：
  - `DeviceError::BlockRequestTooLarge`：块设备不再限制单请求总长度（原上限 1 MiB），改为声明 `SEG_MAX=126`。匹配该变体的分支需要删除。
  - `PlatformError::VmStoppedDuringMemoryTransaction`：由 `PauseOutcome::Stopping` 取代，见“平台接口迁移”。
- `RuntimeError::InvalidIoWidth { width }` 和 `RuntimeError::InvalidMmioWidth { width }` 合并为 `RuntimeError::InvalidAccess { address, width, write }`。
- 行为变化（strict 默认开启，因此默认配置即受影响）：
  - virtio 寄存器区（窗口偏移 `0x100` 以下）的非 32 位读取：以前返回 0，VM 继续运行；现在 strict 下 `run()` 以 `InvalidAccess` 退出，lenient 下返回全 1。配置空间（偏移 `0x100` 起）的 1/2/4/8 字节读取不受影响。
  - virtio 寄存器的非 32 位写入：以前报 `InvalidMmioWidth`；现在 strict 下报 `InvalidAccess`，lenient 下忽略。
  - 宽度不是 1/2/4/8 的访问：以前不论地址一律报 `InvalidIoWidth`；现在起点在已声明窗口内时按 `InvalidAccess` 处理，否则报 `UnmappedIo`。
  - 多字节或偏移 8 及以上的 UART 访问、越过 virtio 窗口末尾的访问：以前报 `UnmappedIo`；现在起点在窗口内即按 `InvalidAccess` 处理。
- 设备树中 UART 的 `reg` 从 `0x1000` 改为 8 字节，与平台布局声明的窗口一致。
- 允许在 DRIVER_OK 之后写 QueueReady=0；以前会报 `QueueConfigurationAfterDriverOk`。

## 多核与动态内存

```sh
./dist/w-vmm run --vcpus 4 --memory-mib 512 \
  --virtio-mem-size-mib 1024 \
  --control-socket /tmp/w-vmm.sock
# 在另一个本地终端：
./dist/w-vmm memory-set --socket /tmp/w-vmm.sock --requested-mib 768
./dist/w-vmm memory-status --socket /tmp/w-vmm.sock
```

核数范围为 `1..=HVF 查询到的上限`，启动后固定。所有 vCPU 在自己的线程上创建、运行和销毁，启动前建立全部 GIC 拓扑。PSCI 0.2 支持 `CPU_ON`、`CPU_OFF`、`AFFINITY_INFO`；可通过客户机 `/sys/devices/system/cpu/cpu1/online` 离线、重新上线次级核。设备后端仍由调用线程独占，无需 `Send` / `Sync`。

`memory_mib` 是不可移除的基础 RAM；virtio-mem 的区域容量和目标容量都是**额外**内存。基础 RAM、动态区域和合计容量均无 16 GiB 固定上限；完整地址范围（含对齐间隙）必须落在当前 VM 的 HVF IPA 位宽内，容量换算与地址计算也必须可表示。设备块大小通过 `--virtio-mem-block-size-mib` 配置（默认 2），显式使用时必须同时指定 `--virtio-mem-size-mib`。块大小为整数 MiB 的二次幂（1、2、4、8……），创建后不可修改，单块映射长度必须可转换为 `usize`。区域容量必须为 `max(128 MiB, block_size)` 的正整数倍，起点位于基础 RAM 后并按同一对齐量向上对齐。目标初始为 0，按设备块大小校验且不能超过区域容量；非法输入直接拒绝、保留原目标，不自动取整。例如容量 128 MiB、块大小 2 MiB 时，6 MiB 合法、3 MiB 非法；块大小 4 MiB 时 6 MiB 非法。128 MiB 仅为区域布局的最低对齐量，平台不探测 guest 热插拔粒度。未启用时保持原有默认启动行为。

库可在 `run` 前取得控制句柄，然后移交给其他线程。

所有设备在 `Vmm::new(config, blocks, net, memory, serial)` 时交付，随后使用 `run()` 或 `run_with_platform(platform)`。内存控制统一使用 `VmControl`；`MemoryControl` 和 `VirtioMem::control()` 仅供内部使用。

`supports_memory()` 表示是否配置 virtio-mem，创建后即确定，停止后保持不变；客户机驱动就绪情况由 `driver_ready` 表示。未配置时，`memory_status()` 和 `set_requested_mib()` 均返回 `virtio-mem is not configured`。启动前调用 `stop()` 会取消运行并立即将内存标记为停止；设备在 Vmm 被消费或丢弃时释放。停止后仍可查询最终内存状态，但不能修改目标。

终端先调用 `Terminal::new()` 取得串口与 guard；创建 VM 后调用 `guard.bind(vm.control())?`，仅允许绑定一次。绑定前收到的退出通知会保留，guard 必须保留到运行结束。控制服务使用 `Server::bind(path, vm.control())`。

```rust,no_run
use w_vmm::{VmConfig, Vmm, VirtioMem};
let memory = VirtioMem::new(1024, 2)?;
let vm = Vmm::new(VmConfig { vcpu_count: 4, ..VmConfig::default() }, blocks, net, Some(memory), serial);
let control = vm.control();
assert!(control.supports_memory());
control.set_requested_mib(6)?;
println!("{:?}", control.memory_status()?);
// 将 control.clone() 交给控制线程，再调用 vm.run()。
# Ok::<(), w_vmm::Error>(())
```

调节成功表示目标已接受，客户机异步尝试扩缩容；设备粒度、区域布局对齐量和 guest 实际调整粒度彼此独立。目标和实际容量允许暂时或长期不一致，不自动视为设备错误，也不会改写目标。`memory_status()?` 包含 `region_size_mib`、`block_size_mib`、`requested_size_mib`、`plugged_size_mib`、`driver_ready` 和 `Created / Running / Stopped` 生命周期。启动前、运行中和暂停期间采用相同校验并允许调节；退出或丢弃未运行的 Vmm 后拒绝调节，句柄保留最终状态。

驱动必须协商 `VIRTIO_MEM_F_UNPLUGGED_INACCESSIBLE`。未插入块不映射到 HVF，也不在设备 DMA 内存视图中。变更映射时暂停所有 vCPU，保护有效队列和待处理缓冲区；失败则回滚，回滚失败终止 VM。普通设备 reset 保留已插入内存及数据，`UNPLUG_ALL` 才移除全部块。

启用动态内存时，Linux 使用 `auto-movable` 上线策略，比例 301%。客户机可能因页面占用而暂时无法缩容，观察目标与实际插入容量差异即可；不保证任意目标立即完成。协议参见 [virtio 1.2](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html)，上线策略参见 [virtio-mem Linux 指南](https://virtio-mem.gitlab.io/user-guide/user-guide-linux.html)。

## VM 外部控制

库根导出 `VmControl`、`VmLifecycle`、`VmStatus`。在运行前通过 `vm.control()` 取得 `Clone + Send + Sync` 句柄，交给控制线程；平台、串口和设备后端仍只在 VMM 线程执行，不要求 `Send` / `Sync`。

```rust,no_run
use w_vmm::{VmConfig, Vmm};
let vm = Vmm::new(VmConfig::default(), blocks, net, None, serial);
let control = vm.control();
// 将 control.clone() 交给控制线程，然后在当前线程调用 vm.run()。
// VM 进入 Running 后，控制线程可以调用：
// control.pause()?;
// control.resume()?;
// control.stop()?;
println!("{:?}", control.status());
```

`pause()` 等待全部 vCPU（含异常处理和 MMIO 回写）与设备处理暂停；`resume()` 等待解除暂停并恢复调度。重复暂停、恢复幂等。暂停期间不处理 guest MMIO、块设备、网络设备或动态内存映射；串口不读取输入，普通输入和 Ctrl-] 留在宿主缓冲区，恢复后才处理。暂停等待仅由控制通知唤醒，没有定时轮询；demo 的 SIGINT、SIGTERM、SIGHUP 和控制停止命令在暂停期间仍有效。内部动态映射使用的短暂暂停不改变对外生命周期。`VmControl::set_requested_mib` 在启动前和暂停期间均可设置，仍只确认宿主目标值，客户机在运行后异步扩缩容。

`stop()` 等待 vCPU 停止、全部磁盘刷新和资源释放，重复调用返回保存的清理结果。它只报告清理失败（磁盘刷新、撤销映射等）；客户机或 vCPU 故障即使与停止同时发生，也只通过 `run()` 返回值和 `final_error` 报告，不会让 `stop()` 失败。它不是客户机正常关机，不包含内存转储或快照恢复。三个阻塞方法均不设超时；从 VMM 运行线程（包括设备回调）调用会立即报错，`status()` 随时可读。

生命周期为 `Created / Starting / Running / Paused / Stopping / Stopped`，状态包含可选的 `final_error`。尚未运行时暂停、恢复报错；启动前停止会取消 VM 并拒绝随后运行，启动中停止在初始化安全边界进入清理。停止优先取消未执行的暂停、恢复请求。暂停与客户机关机或 vCPU 故障竞争时，`pause()` 返回 `ControlError::ReplyFailed("VM is stopping")` 并立即进入清理，正常关机仍以成功结束，真实故障不会被这条停止错误掩盖。启动失败、运行错误、客户机关机和丢弃未运行的 VM 均更新最终状态并通知等待者；暂停或恢复失败也会终止并清理。

`run()` 与 `final_error` 的错误优先级：运行主错误（启动、设备、轮询等）> 平台 `stop` 返回的 vCPU 故障 > 清理错误。无论前面是否已有错误，停止 vCPU、刷新每块磁盘、撤销动态和基础映射都会全部尝试。

```sh
# 无需配置 virtio-mem
./dist/w-vmm run --control-socket /tmp/w-vmm.sock
# 另一个本地终端
./dist/w-vmm status --socket /tmp/w-vmm.sock
./dist/w-vmm pause --socket /tmp/w-vmm.sock
./dist/w-vmm resume --socket /tmp/w-vmm.sock
./dist/w-vmm stop --socket /tmp/w-vmm.sock
```

控制 socket 只在显式指定时创建，权限 `0600`，拒绝覆盖已有路径，退出时按 inode 身份清理自身 socket。每连接一条换行 JSON 请求及响应，上限 4096 字节；请求接收和响应写入限时 500 ms，同步操作及客户端等待完成不设超时。服务最多使用 16 个并发连接处理线程，退出前等待连接响应完成再清理 socket。

JSON 命令为 `{"command":"pause"}`、`resume`、`stop`、`status`，成功返回 `{"ok":true,"status":{"lifecycle":"Paused","final_error":null}}`。`{"command":"memory-set","requested_mib":768}` 和 `{"command":"memory-status"}` 保持原有响应字段；未配置内存设备时明确报错。所有错误返回 `{"ok":false,"error":"..."}`，CLI 输出 JSON，失败退出码非零。不要在启动前遗留同名 socket；不会自动删除他人文件。

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
# 需要本地 qemu-img：压缩、稀疏、跨簇、只读、越界、持久化、空段、IOV_MAX 分批及重叠读取
cargo test -p w-vmm --test volatile_disk --locked -- --ignored
cargo build -p w-vmm --lib --locked
./build.sh
python3 scripts/smoke.py --binary dist/w-vmm
```

单元测试覆盖串口双向字节、FIFO 容量、空输入、读写/刷新错误、控制字节直通和 trait 对象，以及设备树与多设备地址分配、公共 MMIO 协商/复位/双队列/中断、描述符校验、多盘状态隔离、磁盘 I/O 错误、网络分散缓冲收发、背压重试、短缓冲丢包和后端错误，以及 strict/lenient 错误分类、寄存器写入原子性、设备隔离与复位、停用队列（含挂起的网络 TX）被通知/轮询/pin 扫描跳过、宿主错误始终致命和“先更新中断再完成 I/O”的顺序。

冒烟脚本仅创建临时测试盘，日志保存在 `test-results/`。执行真实 HVF 启动、shell 命令、多盘 serial 映射与独立持久化、ext4 挂载、192 KiB 随机数据写入与 SHA-256 校验、同步/卸载/关机/重启读回、只读镜像哈希不变、只读盘上 `max_segments` 为 126 且 4 MiB 读取（含把 `max_sectors_kb` 提到硬件上限后）正常完成、重复打开锁冲突、损坏文件、宿主文件大小限制导致的真实 EFBIG、重启退出和信号/快捷键后的终端恢复。关机后调用 `qemu-img check`。

离线测试把可执行文件和数据盘放入仅含这两个文件的临时目录启动。未指定网络后端时不会创建宿主网络接口；运行时不会调用外部工具。`otool -L` 的链接项包括 Hypervisor.framework、vmnet.framework 和 macOS 系统库。验收宿主为 macOS 27.0 / Apple Silicon，尚未在 macOS 15 真机上回归。

### 多核与内存验收

```sh
# guest-probe 仅用于测试，需要本地 aarch64-linux-musl-gcc、qemu-img、e2fsprogs。
python3 scripts/smoke-memory.py --binary dist/w-vmm
python3 scripts/smoke-control.py --binary dist/w-vmm
cargo build -p w-vmm-demo --example net-peer --locked
codesign --force --sign - --entitlements assets/entitlements.plist target/debug/examples/net-peer
python3 scripts/smoke-net.py --peer --memory --binary target/debug/examples/net-peer
```

控制冒烟使用临时测试盘，验证 1/4 核反复暂停恢复、离线核、磁盘负载、UART 输入保留、Ctrl-] 延后处理、并发状态读取、暂停时停止和信号退出、并发信号与 socket stop、启动失败及正常关机回收、socket 清理，以及暂停期间设置内存目标。

内存冒烟还验证 2 MiB 块的 `0 → 6 → 2 → 0`、4 MiB 块的 `0 → 12 → 4 → 0`，包括暂停期间设置目标。1 MiB 配置会记录 guest 粒度及目标 3 MiB 的实际插入量，不要求收敛，再验证可达目标 4 MiB、归零和停止。2026-09-20 本地 HVF 验证中，随附 guest 报告 128 MiB memory block、2 MiB subblock；设备块 1 MiB、目标 3 MiB 时，观察到实际 2 MiB，目标保持 3 MiB，随后 4 MiB、0 和停止均成功。

内存冒烟使用临时测试盘，验证 1/2/4 核、每核绑核计算、次级核离线/上线、4 核重启/信号/快捷键与终端恢复。客户机反复扩容、写入并校验 640 MiB（超过基础 RAM）、缩容到零、再扩容，同时核对控制状态、`MemTotal` 和宿主 RSS 回收。组合测试在 4 核和动态内存反复扩缩容时执行本地网络及双盘 I/O，不使用 vmnet 或外部网络。

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
- `src/platform/macos_arm64/mod.rs`：`VmRuntime` 的 macOS/arm64 实现——调用线程上的设备事件循环与串口中断接线；`#[cfg]` 门控，仅 Apple Silicon 编译。
- `src/platform/macos_arm64/hvf.rs`：最小 FFI、VM 映射、原生 GIC 和绑定创建线程的 RAII vCPU。映射表持有每个已映射区域的 `Arc<GuestRegionMmap>`，撤销映射时校验 guest 地址、长度与分配身份，仅在 `hv_vm_unmap` 成功或 VM 销毁成功后释放；`hv_vm_destroy` 失败时有意泄漏仍映射的分配。
- `src/devices/mmio.rs`：共享 modern virtio-mmio、功能协商、队列配置、描述符校验、复位及中断确认；寄存器写入先校验后提交，lenient 模式下隔离出错设备（DEVICE_NEEDS_RESET）。
- `src/platform/macos_arm64/cpus.rs`：每核独立线程、PSCI 启停、全核暂停及统一退出。MMIO 写值按访问宽度截断；未停止时回复通道断开属于 vCPU 故障。
- `src/devices/memory.rs`：公开 `VirtioMem`、可跨线程克隆的控制句柄与生命周期；virtio-mem 请求与按配置块大小事务映射、回滚、回收。
- `demo/src/control.rs`：可选本地 Unix socket JSON 服务与客户端。
- `src/devices/block.rs`：磁盘请求与配置，单个 128 项 split virtqueue，IN/OUT/FLUSH/GET_ID；声明 `SEG_MAX=126`（队列长度减去头部和状态描述符），不声明 `SIZE_MAX`，不限制单请求总长度。
- `src/devices/net.rs`：网络请求与配置，RX/TX 各 128 项队列，完整帧收发与发送背压。
- `src/net.rs`、`src/net/macos.rs`：`NetDevice` trait 与 macOS vmnet NAT 后端。
- `src/storage.rs`：imago 原生同步 qcow2、同 inode 文件锁、范围校验、内部缓存 flush 与一次宿主 sync（imago 对同一 inode 执行 `sync_all`，macOS 上为 F_FULLFSYNC）。
- `src/serial.rs`：`SerialIo` trait、Box 转发和 FIFO 输入轮询。
- `demo/src/terminal.rs`：原始终端、非阻塞输入、Ctrl-] 与信号恢复，CLI 和网络示例共用。
- `src/lib.rs`：`VmConfig` / `Vmm::new(config, blocks, net, memory, serial).run()`，委托 `platform::run`。
- `demo/src/main.rs`：CLI、错误输出，通过路径依赖调用根库。

设备 MMIO：GIC distributor `0x08000000`，16550 `0x09000000`（SPI 33），virtio 设备从 `0x0a000000` 开始，每个占 `0x1000`，中断从 SPI 34 递增。先按名称排列磁盘，再放置可选网卡和 virtio-mem，每个设备使用独立 MMIO 和中断。GIC redistributor 位于 `0x10000000`，RAM 位于 `0x40000000`。实际可用 SPI 范围、定时器 INTID 和 redistributor 空间大小查询 HVF；设备超出范围时启动报错。

目前无图形、快照、CPU 热添加或其他宿主平台。设备事件循环空闲等待最多 2 ms，网络每轮每方向最多处理 64 个包，存储请求同步执行，未做吞吐/延迟优化。默认 strict 模式下，非法设备访问和损坏的队列结构会报错并退出；lenient 模式下的处理见“客户机错误处理”，宿主错误在两种模式下都会退出。磁盘请求的状态描述符可信（最后一个描述符为 1 字节、设备可写且位于 guest 内存内）时，格式错误的请求和 I/O 错误返回 virtio IOERR 并记录原因，未知操作返回 UNSUPP，两者都正常完成，与 strict 无关；状态描述符不可信时，strict 下退出，lenient 下隔离该块设备（置 `DEVICE_NEEDS_RESET`）。损坏的队列或描述符链（地址越界、非法标志、间接描述符、环路等）在到达请求校验之前就会被拒绝：strict 下退出，lenient 下隔离该设备。此实现尚未经过不可信客户机的安全审计。

第三方来源与许可见 [THIRD_PARTY.md](THIRD_PARTY.md)。

## 第三方平台接入

公共库支持 Linux / macOS 的 ARM64 与 x86_64。`run()` 在 Apple Silicon 上使用内置 HVF，其他目标返回错误并提示使用 `run_with_platform()`。`boot` API 和嵌入的 ARM64 镜像仅在 Apple Silicon macOS 上编译；demo 保持使用默认 HVF。

完整可运行实现和调用见 [`examples/platform.rs`](examples/platform.rs)：

```sh
cargo run -p w-vmm --example platform
```

此示例通过端口 UART 输出一行文字后关机，无需虚拟化权限。外部项目需要 `vm-memory = { version = "0.18", features = ["backend-mmap"] }` 和 `w-vmm`。覆盖多个 RAM 区间、virtio 队列、动态内存映射及故障注入的完整实现见 [`tests/platform.rs`](tests/platform.rs)，它仅使用公开 API。

实现 `platform::Platform` 的 `layout` 和 `create`，关联的 VM 实现 `memory::Mapper` 与 `platform::VirtualMachine`。布局需求按磁盘名称排序，然后是网络、动态内存；返回相同顺序的 virtio-MMIO 区间。库校验地址溢出、区间重叠、RAM 总容量、设备数量与热插拔容量/对齐。端口与 MMIO 属于不同地址空间；UART 使用八个连续的字节寄存器。平台还须校验自身支持的 CPU 数量、映射粒度和 IRQ 范围。

`prepare` 接收已映射的基础 RAM 和布局，由平台加载客户机、设置启动寄存器和中断控制器。`start` 启动执行，`poll_event` 返回 I/O、关机或轮询错误；无事件返回 `None`。I/O 的完成凭据由平台定义，库处理设备后调用 `complete_io`，平台回填寄存器并推进指令。平台与设备后端不要求 `Send` / `Sync`，线程约束由平台内部处理。

`Mapper::map/unmap` 单次失败必须不留部分变更。`map` 接收 `Arc<GuestRegionMmap>`，成功后由平台 VM 持有该分配，直到 `unmap` 成功或 VM 销毁；库和调用方可以随时丢弃自己的引用。`unmap` 必须校验 guest 地址、长度及分配身份（`Arc::ptr_eq(&a.get_mmap(), &b.get_mmap())`，包装同一分配的另一个 `GuestRegionMmap` 视为同一映射），失败时继续持有分配；VM 销毁失败时宁可泄漏也不能释放仍被引用的内存。回滚未能撤销的映射同样由平台持有，库不再另行保留。

`pause` 返回 `PauseOutcome::Paused` 表示全部 vCPU 已静止；VM 已因客户机关机或 vCPU 故障进入停止时返回 `PauseOutcome::Stopping`，此时库不会调用 `resume`：动态映射事务直接结束，宿主暂停请求以 "VM is stopping" 失败，运行随即进入清理。动态映射成功或回滚成功后恢复，回滚失败直接进入停止流程。`stop` 必须先停止并回收全部 vCPU（支持部分启动），再取出全部待处理事件，返回第一个尚未由 `poll_event` 报告的 vCPU 故障；重复调用返回 `Ok`。正常关机、运行错误、启动失败、宿主停止都经过同一次 `stop`。VM 析构也必须支持重复清理。清理顺序为停止 vCPU、尝试刷新每块磁盘、撤销映射、销毁 VM，最后释放基础 RAM 与动态 RAM。

### 平台接口迁移

自定义平台从上一版本升级时：

| 旧接口 | 新接口 | 迁移要点 |
| --- | --- | --- |
| `fn map(&mut self, region: &GuestRegionMmap)` | `fn map(&mut self, region: Arc<GuestRegionMmap>)` | 在映射表中保存 `Arc`，`unmap` 成功后移除，VM 析构时释放；不要再依赖调用方保持分配存活 |
| `fn unmap(&mut self, region: &GuestRegionMmap)` | 签名不变 | 增加地址、长度和分配身份校验；失败时保留表项 |
| `fn pause(&mut self) -> Result<()>` | `fn pause(&mut self) -> Result<PauseOutcome>` | 原先“暂停时已停止”返回的错误改为 `Ok(PauseOutcome::Stopping)`；正常暂停返回 `Ok(PauseOutcome::Paused)` |
| `fn stop(&mut self)` | `fn stop(&mut self) -> Result<()>` | 先回收全部 vCPU 再排空事件，返回第一个未报告的故障；重复调用返回 `Ok` |
| `PlatformError::VmStoppedDuringMemoryTransaction` | 已删除 | 由 `PauseOutcome::Stopping` 取代 |

内置 HVF 平台新增的 `PlatformError` 变体：`MmioReplyDisconnected`（未停止时 MMIO 回复通道断开）、`MappingOverlap`、`MappingNotFound`、`MappingMismatch`（映射表校验）。`VmControl::stop()` 只返回清理结果；执行故障（包括只在平台 `stop` 时才发现的 vCPU 故障）通过 `run()` 返回值和 `status().final_error` 报告。完整示例见 [`examples/platform.rs`](examples/platform.rs)。

`memory::VirtioMem` 封装私有设备，库根的 `VirtioMem` / `MemoryStatus` / `MemoryLifecycle` 导出保持兼容。设备块大小由使用者配置，容量和区域起点按 `max(128 MiB, block_size)` 对齐，目标按设备块大小校验；平台不推断或配置 guest 热插拔块大小。多架构接口不保证任意 guest 内核的热插拔兼容性。
