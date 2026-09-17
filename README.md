# w-vmm

在 Apple Silicon 上通过 macOS Hypervisor.framework 直接启动 Alpine ARM64 的独立 Rust VMM。1 个 vCPU，默认 512 MiB RAM，原生 GICv3，16550 串口 shell，可选 qcow2 数据盘。内核和 initramfs 编译进可执行文件；客户机根文件系统驻留内存。

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

准备脚本下载固定 SHA-256 的 Alpine 3.24.1 aarch64 minirootfs 和 `linux-virt-6.18.52-r0.apk`。内核与模块来自同一个包。脚本解析 EFI zboot gzip 封装生成 ARM64 Image，选择 virtio-mmio、virtio-blk、ext4 及模块依赖，在 Mac 上生成确定性 newc/gzip initramfs。BusyBox init 作为 PID 1 回收子进程、启动串口 shell 并负责关机。

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
use w_vmm::{VmConfig, Vmm};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    Vmm::new(VmConfig::default()).run()?;
    Ok(())
}
```

`VmConfig` 可配置 `disk`、`memory_mib` 和 `read_only`。内核、initramfs、终端与信号处理均由库负责；调用程序仍需满足上述平台要求，并在运行前附加相同的 Hypervisor entitlement 本地签名。

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
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p w-vmm --lib --locked
./build.sh
python3 scripts/smoke.py --binary dist/w-vmm
```

单元测试覆盖内存布局/越界、设备树基本结构、virtio 协商/复位/used ring/中断确认、描述符环路与非法地址、磁盘越界/只读和注入 I/O 错误。

冒烟脚本仅创建临时测试盘，日志保存在 `test-results/`。执行真实 HVF 启动、shell 命令、ext4 挂载、192 KiB 随机数据写入与 SHA-256 校验、同步/卸载/关机/重启读回、只读镜像哈希不变、重复打开锁冲突、损坏文件、宿主文件大小限制导致的真实 EFBIG、重启退出和信号/快捷键后的终端恢复。关机后调用 `qemu-img check`。

离线测试把可执行文件和数据盘放入仅含这两个文件的临时目录启动。程序没有联网或运行外部工具的代码；`otool -L` 仅列出 Hypervisor.framework、libSystem 和 libiconv 等 macOS 系统库。验收宿主为 macOS 27.0 / Apple Silicon，尚未在 macOS 15 真机上回归。

## 代码结构

- `Cargo.toml`：根 library 包和 workspace 入口，共用 `Cargo.lock`、`target/` 及 release profile。
- `demo/Cargo.toml`：不可发布的 `w-vmm-demo` 包，声明 `w-vmm` 二进制及 CLI 依赖。
- `src/boot.rs`：校验 ARM64 Image、RAM/内核/initramfs/FDT 布局，linux-loader 加载及 vm-fdt 设备树。
- `src/platform/mod.rs`：`VmRuntime` trait 与按构建目标的运行时选择（`Runtime` 别名、`run()` 分派）。
- `src/platform/macos_arm64/mod.rs`：`VmRuntime` 的 macOS/arm64 实现——HVF 运行循环、vCPU 唤醒（Kicker）与串口中断接线；`#[cfg]` 门控，仅 Apple Silicon 编译。
- `src/platform/macos_arm64/hvf.rs`：最小 FFI、VM 映射、原生 GIC 和绑定创建线程的 RAII vCPU。VM 内存由 vm-memory 持有，先销毁 vCPU 和映射，再释放 RAM。
- `src/devices/block.rs`：modern virtio-mmio，单个 128 项 split virtqueue，同步顺序处理 IN/OUT/FLUSH/GET_ID，单请求上限 1 MiB。
- `src/storage.rs`：imago 原生同步 qcow2、同 inode 文件锁、范围校验、内部缓存 flush 与宿主 sync/fsync。
- `src/terminal.rs`：原始终端、非阻塞输入与信号恢复。
- `src/lib.rs`：`VmConfig` / `Vmm::new(...).run()`，委托 `platform::run`。
- `demo/src/main.rs`：CLI、错误输出，通过路径依赖调用根库。

设备 MMIO：GIC distributor `0x08000000`，16550 `0x09000000`（SPI 33），virtio-blk `0x0a000000`（SPI 34），GIC redistributor `0x10000000`，RAM `0x40000000`。定时器 INTID 和 redistributor 空间大小查询 HVF，避免假定宿主参数。

初版无网络、图形、SMP、快照、热插拔或其他宿主平台。轮询唤醒周期为 10 ms，存储请求同步执行，未做吞吐/延迟优化。非法设备访问/队列结构会报错并退出；普通磁盘请求 I/O 错误返回 virtio IOERR 并记录原因。此实现尚未经过不可信客户机的安全审计。

第三方来源与许可见 [THIRD_PARTY.md](THIRD_PARTY.md)。
