# w-vmm 问题清单与改进建议

评审基线：`87ed12f`，日期 2026-10-23。

## 背景与结论

最紧迫的问题：guest 执行一条普通的 `dd bs=4M`，整个 VM 就会退出，已实机复现。草案原定首要的“WFI 空转”经实测不成立，已放弃。

评审流程：

1. 通读源码，写出改进草案 P1–P7。
2. 通过 Orca 编排 codex、claude 两个只读 worker，逐条对照源码核对。claude 还用 `--read-only` 磁盘实机启动了 guest。
3. 两份评审的关键结论均已重新对照源码复核。

下文“双方一致”指 codex 与 claude 结论相同；其余条目标明由哪一方提出。

## 实施状态（2026-10-23，工作区未提交）

**当前结论：** 第 1–3 批均已完成，经独立评审验收，并在最终 release 二进制上通过了实机冒烟（证据见下）。以下改动均在工作区，尚未提交。剩余工作见第六节“剩余范围”；S2（imago 共享内存别名）仍是有意接受的已知限制。本文不构成安全审计结论，此实现尚未针对不可信客户机做过全面的安全评估。

- **第 1 批（已完成，已验收）：** D1、D4、D5，S2 写入文档。
- **第 2 批（已完成，已验收）：** D2、D3；完成 S1：`Mapper::map` 接收 `Arc`，HVF 映射表持有分配，并校验地址、长度和分配身份。`hv_vm_destroy` 失败时先保住泄漏、再记录日志。MMIO 写值按宽度截断；未停止时回复通道断开视为故障；`VirtioMem::stop` 的撤销映射错误计入清理结果。协调者曾在第 2 批冻结的二进制上实机跑过 `smoke-control.py`，以及 4 MiB 读取、提高队列请求上限、`SEG_MAX=126` 和 8 次并发扩缩容/停止的验收。
- **第 3 批（已完成，已验收）：** 完成第三节的错误分层和 `VmConfig::strict`（默认 `true`，demo 用 `--lenient` 关闭）、寄存器“先校验后提交”、D6、D7、UART 设备树窗口改为 8 字节；`scripts/smoke.py` 的只读盘用例补充了 D1 的 4 MiB 回归检查。具体规则见 README“客户机错误处理”。

### 最终验证（2026-10-23，协调者独立执行）

静态检查与测试：

- `cargo fmt --all -- --check`：通过。
- `cargo clippy --workspace --all-targets --locked -- -D warnings`：通过。
- `cargo test --workspace --locked`：88 通过，3 忽略（1 个需要 vmnet 权限，2 个依赖 qcow2 外部工具）。
- `volatile_disk` 的 `--ignored` 用例单独运行：2 通过。

实机冒烟，宿主为 Apple Silicon / macOS 27。被测对象是 `./build.sh` 重新构建并签名的 `dist/w-vmm`（SHA-256 `dd7ba71538b83bc190227d058a77aae88443b59d54ec6e60491195629ca5fb95`），与此前已完整冒烟的 release 二进制逐字节一致：

- `scripts/smoke.py`：通过。包括新增的 4 MiB 读取、`max_segments` 为 126、提高请求上限后的读取、只读镜像哈希不变、持久化、真实 EFBIG、关机和信号处理。
- `scripts/smoke-control.py`：通过。
- `scripts/smoke-memory.py`：通过。覆盖 1/2/4 vCPU、1/2/4 MiB 设备块、640 MiB 数据、内存回收和 32 GiB 稀疏区域。
- `/tmp/w-vmm-plan-acceptance.py dist/w-vmm --stop-race`（仓库外的临时验收脚本）：默认 strict 与 `--lenient` 各自通过，每种模式 8 次并发扩缩容/停止，共 16 次；同时验证 4 MiB 读取、`max_segments=126`、提高请求上限和关机。
- `scripts/smoke-net.py --peer --memory --binary target/debug/examples/net-peer`（被测对象是 debug 构建的 net-peer 示例，不是 `dist/w-vmm`；使用原有 Terminal 实现）：0 盘和 2 盘均通过。最初几次运行失败，根因在测试脚本：脚本没有读取 PTY，热插拔期间的 printk 写满输出，串口后端返回 `WouldBlock`，按设计作为宿主错误退出；另外 printk 会把命令标记拆开。修复只改测试脚本（主线程排空 PTY、测试 guest 中执行 `dmesg -n5`、限定异常诊断输出），没有修改运行时或 `demo/src/terminal.rs`，也不是运行时或安全修复。修复后实现 worker 连续 3 次通过，协调者直接调用 1 次通过。早期一组基于实验性 Terminal 改动的 25 次运行已作废，不作为证据。

未覆盖的范围：

- 未运行真实 vmnet / NAT / TCP 网络测试（需要 vmnet 权限）。
- 未在 macOS 15 上回归。
- 写入测试只使用临时创建的磁盘，没有对已有磁盘做写入测试。

下文各条保留评审时的原始描述，并在需要时注明后来的处理结果。

## 一、已确认缺陷

D1、D4、D5 在第 1 批修复，D2、D3 在第 2 批修复，D6、D7 在第 3 批修复。

| # | 问题 | 位置 | 后果 | 修复方向 | 来源 |
| --- | --- | --- | --- | --- | --- |
| D1 | 超过 1 MiB 的块请求被当作致命错误，且设备未声明 `SIZE_MAX/SEG_MAX` | `src/devices/block.rs:94-99`、`:23-38` | guest 执行 `dd if=/dev/vda of=/dev/null bs=4M` 即退出（已复现）；普通预读也可能触发 | 声明 `size_max`/`seg_max`；超限请求返回 IOERR 并 complete | 双方一致 |
| D2 | 停止时 `poll_event` 只 `try_recv` 一次：如果先取到 `Access`，后面的 `Failed` 就丢了 | `src/platform/macos_arm64/mod.rs:153-158` | vCPU 的真实故障被报成正常 Shutdown，`run()` 可能返回 Ok | 循环排空 channel，优先返回 `Failed` | 双方一致 |
| D3 | pause 与 stop 竞争时返回 `VmStoppedDuringMemoryTransaction`，runtime 直接 `?` 返回 | `cpus.rs:149-151`、`runtime.rs:155-164`、`:206` | 正常 poweroff 被报成失败；真实错误留在 channel 里无人读取 | pause 返回“已在停止”信号，runtime 继续排空事件，以其中的结果为准 | claude |
| D4 | 后端回调 panic 时，`Runtime` 字段按声明顺序析构，动态 RAM 早于 VM 释放 | `src/runtime.rs:40-46` | vCPU 仍在运行，宿主内存页却已释放 | 把 `vm` 移到 `devices`/`base` 之前声明，用 Drop 探针测试顺序 | codex |
| D5 | 每次 guest FLUSH 执行两次 F_FULLFSYNC | `src/storage.rs:285-298` | 同一 inode 冗余同步一次（延迟影响未实测） | 确认是同一 inode 后，删掉重复的 `sync_all` | claude |
| D6 | DRIVER_OK 后拒绝所有队列配置写入，连 `QueueReady=0` 也拒绝 | `src/devices/mmio.rs:324-330` | 不符合 virtio 1.2 MMIO 规范中的队列停用流程 | 允许停用队列，`poll` 跳过未就绪的队列 | codex |
| D7 | 中断电平在回复 vCPU 之后才更新 | `runtime.rs:172-183`、`:271` | guest EOI 后 SPI 仍为高电平，产生伪中断 | 处理 InterruptACK、UART IIR 之后，先 `set_irq` 再 `complete_io` | claude |

D1 最终的修复方式：只声明 `SEG_MAX=126`，不声明 `SIZE_MAX`，也不限制单请求长度。D6 的最终范围：DRIVER_OK 之后只允许 `QueueReady=0`；停用的队列被通知、轮询和 pin 扫描跳过，网络挂起的 TX 帧也不再重试；不支持在 DRIVER_OK 之后重新启用队列。

较小的问题：

- ~~设备树中 UART 的 `reg` 是 `0x1000`~~：第 3 批改为 8 字节，与 layout 一致。
- ~~MMIO 回复通道断开时，vCPU 静默返回 `Ok(())`~~：第 2 批起，未停止时报 `MmioReplyDisconnected`。
- ~~MMIO 写值没有按访问宽度截断~~：第 2 批已修复。
- PSCI SYSTEM_RESET 被当作关机处理；HVC32 未处理。**未处理。**
- `VmControl::stop()` 没有超时；demo 控制 socket 最多 16 个阻塞客户端，占满后 `status` 也无法接入（`demo/src/control.rs:152-164`）。**未处理。**
- 清理结果不完整：`VirtioMem::stop` 的错误已计入清理结果，`hv_vm_destroy` 失败会泄漏映射并记录日志（第 2 批）；vmnet Drop 仍只打印错误。

## 二、健全性（unsafe / 生命周期）

- **S1 `Mapper::map` 公开 API 不健全（双方一致）。已在第 2 批修复。** `src/memory.rs:15-21` 中，region 的生命周期只靠文档约束；`hvf.rs:188-212` 只把裸指针交给 HVF，没有持有 region。纯安全代码就能做到“map 后 drop region”，构成 UAF。建议签名改为接收 `&Arc<GuestRegionMmap>`，由 VM 持有到 unmap 或 destroy；属于公开 API 变更。
- **S2 imago 0.2.5 把 guest 共享内存转成普通 slice（codex）。** 位置：`io_buffers.rs:960/1053` 的 `from_raw_parts(_mut)`，zero-fill 路径 `format/access.rs:208` 的 `fill`。其他 vCPU 仍可同时访问这些页，在 Rust 别名模型下属于 UB。**决定：保留零拷贝，在 README 和 `src/storage.rs` 中写明这是已知限制，等待上游修复。**
- **S3 跨线程错误降级为字符串**（`control.rs:56`、`:213-243`）。diagnostic 保留了 source 链，但调用方无法按错误类型匹配。可考虑 `ControlError::ReplyFailed(Arc<Error>)`，优先级低。**未处理。**

## 三、健壮性：guest 错误分层

现状：未映射 MMIO、非法访问宽度、驱动状态机错误、描述符非法，都会经 `?` 冒泡导致整个 VM 退出。只有块后端的 I/O 错误被转成 IOERR。

**前置条件（claude 发现）：** 寄存器写入是“先提交后校验”，必须先改。`mmio.rs:304` 先执行 `self.status = value` 再校验，`:346` 先 `set_ready(true)` 再 `is_valid`。另外 `pop` 会在校验之前推进 `next_avail`，所以出错的链已被消费，只能对设备做 reset，不能仅记日志后继续。

| 错误类别 | 建议处理 |
| --- | --- |
| 单个块请求非法，但 status 描述符可信 | 写 IOERR/UNSUPP 并 complete |
| 队列或描述符已损坏 | 对该设备置 `DEVICE_NEEDS_RESET (0x40)`，发配置变更中断（复用 `config_changed`），停止其数据面（`active()` 已排除 0x40） |
| 寄存器协议错误 | 忽略本次写，计数并限流记录日志 |
| 未映射 MMIO | 设备树已声明的窗口内读返回全 1、写忽略；其他地址维持 fail-stop |
| 宿主错误（HVF 调用失败、映射回滚失败） | 维持 fail-stop |
| 未知 sysreg 或异常 | 暂不改；以后可考虑向 guest 注入 UNDEF，不建议一律 RAZ/WI |

另外新增 `VmConfig { strict: bool }`：测试和 fuzz 时保持 fail-stop，便于尽早暴露 bug。

**最终实现（第 3 批，取代上表中的建议）：**

- `strict` 默认为 `true`，保留原有的 fail-stop 行为；demo 用 `--lenient` 关闭。错误按变体穷举分类（`DeviceError::is_guest`、`MemoryError::is_guest`），新增变体时编译器会要求明确归类。
- lenient 模式下：
  - 非法寄存器写入被拒绝且不产生副作用，按 2 的幂次限流记录日志。
  - 损坏的队列、不可信的描述符（包括不可信的块状态描述符、非法网络帧、非法 virtio-mem 请求缓冲）只隔离对应设备：置 `DEVICE_NEEDS_RESET`，停止其数据面；已设置 DRIVER_OK 时发送配置变更中断。该位只能由设备复位清除。
  - 设备窗口内无法解码的访问（非法宽度、virtio 寄存器非 32 位、UART 非单字节）读返回全 1、写忽略；起点在已声明窗口之外的访问在两种模式下都致命。
- 两种模式都致命：逃逸出来的宿主错误、后端违反接口约定、映射回滚失败。映射失败但完整回滚成功时，仍向 guest 返回失败响应。块请求的 IOERR/UNSUPP 与 strict 无关，始终正常完成。
- 状态写入先校验后提交。不支持的特性仍通过清除 FEATURES_OK 拒绝；同一次写入若带 DRIVER_OK，则整次写入被拒绝。QueueReady=1 校验失败时恢复为未就绪。
- 动态内存事务逐个设备扫描 pin：lenient 下出错的设备被隔离并从 pin 中排除；virtio-mem 自身被隔离时跳过处理并恢复 vCPU。
- 原先“未知 sysreg 或异常”一行未改动。

## 四、性能与功能

- **P1 WFI 空转：否决。** VMM 使用 HVF 内核 GICv3（`hv_gic_create`），vtimer 以 PPI 27 由内核注入。实测 2 vCPU guest 空闲时，整个进程约占 1% 核，vCPU 线程的 CPU 时间接近 0。如果在用户态实现定时睡眠，内核 vGIC 内部投递的 IPI 唤醒会丢失。替代做法：增加 exit 计数，并在 macOS 15 上复测。
- **P2 主循环 2 ms 轮询：部分成立。** MMIO 处理不会叠加 2 ms，因为 `recv_timeout` 收到事件会立即返回。真正受 2 ms 影响的是 vmnet RX 和串口输入，空闲开销约 1% 核。可以做的改进：
  - 有界地批量处理事件，并在批次之间检查 stop/pause；
  - 缓存中断电平，只在变化时调用 `set_irq`；
  - 统一唤醒源，例如在 vCPU 的 mpsc 中新增 `Event::Wake`，配合 vmnet 事件回调。
- **P3 块 I/O 阻塞 VMM 线程：事实成立，但成本被低估。**
  - `Disk` 本身大概率是 `Send + Sync`，Send 约束不是真正的障碍。
  - 真正的难点：virtio-mem 的 pin 集合（`Queues::pinned`）只覆盖未 pop 的链，in-flight 请求不在其中；pause 屏障也需要扩展到 I/O 线程。
  - 建议顺序：先做 D1、D5，再做“posted notify”（先回复 vCPU、再按预算处理队列），worker 线程最后做。
- **P5 virtio 特性。** `SEG_MAX/SIZE_MAX` 属于正确性问题，已并入 D1。后续可依次考虑：
  - 队列长度提到 256，加 INDIRECT_DESC；
  - EVENT_IDX：在同步模型下收益有限；
  - DISCARD/WRITE_ZEROES：需要扩展 `BlockStorage` trait；
  - 网卡 offload：依赖 vmnet 支持，且需改造 trait，放到最后。
- **P6 API。**
  - BootSource：内核和 initrd 通过 `include_bytes!` 写死，bootargs 固定（`boot.rs:17-18`、`:135-141`）。`assets/` 被 gitignore，干净 checkout 后连 `cargo check` 都通不过，这也阻塞 CI。建议引入 `BootSource { kernel, initrd, cmdline }`，内置资产放到 `bundled-guest` feature 后面。
  - virtio-mem 必须是最后一个设备的隐式约定（`runtime.rs:198-205`）：改为显式保存 `mem_index`。
  - `Vmm::new` 有 5 个位置参数，调用时还要写 `None::<Vmnet>`：可提供简化构造或 builder，与 BootSource 合并为一次 breaking release。

## 五、工程化

- 没有仓库内 CI。现有 cargo 测试都不依赖 HVF；fmt 和 clippy 当前无告警，接入 `-D warnings` 几乎零成本。阻塞 CI 的是缺少启动资产（见 P6）。HVF 冒烟测试需要自托管的 Apple Silicon runner。
- 库中 4 处直接调用 `eprintln!`（`block.rs:165`、`storage.rs:307`、`devices/memory.rs:201`、`net/macos.rs:339`），建议改用 `log` facade，并增加 exit 与中断计数。
- 测试缺口：
  - ~~没有 panic 与析构顺序的测试（D4）~~：已补 `tests/platform.rs` 的 `backend_panic_destroys_vm_before_releasing_ram`。
  - `cpus.rs` 的异常解码和 HVF 调用混写在一起。第 2 批已把 MMIO 写值截断（`mask_write`）和回复等待（`wait_reply`）抽成函数，并补了截断、回复通道断开、回复不跨越暂停屏障三项单元测试；异常解码整体抽成纯函数并配表驱动测试**仍未做**。
  - 没有“任意 guest 输入都不导致致命错误”的性质测试或 fuzz（本可提前发现 D1）。**未处理。**
  - 没有“virtio-mem 拔出与 I/O 并发”的自动化测试。现有单元测试只覆盖停用队列不参与 pin 扫描；第 2 批的实机验收覆盖的是扩缩容与停止的竞争，不是拔出与 in-flight I/O 的竞争。**未处理。**
- 未提交的 signal-hook 0.3→0.4 升级（Cargo.lock 中还带有 unicode-ident 1.0.26）：编译已通过。第 2 批和最终 release 二进制均已通过 `scripts/smoke-control.py`（包括信号停止）；仍需单独提交。

## 六、路线图与已做决定

已做决定：

- imago 零拷贝（S2）保留，只做文档化。
- P1 放弃。

建议实施顺序：

1. **阶段 1：已完成（第 1、2 批）。** 第 1 批修复 D1、D4、D5 并完成 S2 的文档化，第 2 批修复 D2、D3。
2. **阶段 2：已完成（第 2、3 批）。** 第 2 批完成 S1（Mapper 接收 Arc）、MMIO 写值截断和回复通道断开；第 3 批完成寄存器“先校验后提交”、第三节的错误分层与 strict 开关，以及 D6、D7 和 UART 窗口。
3. **剩余范围：**
   - 上文仍标注“未处理”的小问题：PSCI SYSTEM_RESET/HVC32、`VmControl::stop()` 超时、demo socket 客户端上限、vmnet Drop 只打印错误、S3；
   - virtio-mem 仍按“最后一个设备”隐式定位（P6 的 `mem_index`）；
   - “任意 guest 输入都不导致致命错误”的性质测试或 fuzz（lenient 模式现已具备前提）；
   - BootSource 与 `bundled-guest` feature；
   - CI；
   - `log` 与 exit 计数；
   - `cpus.rs` 异常解码整体抽成纯函数并补表驱动测试（MMIO 截断、回复等待和暂停屏障已有测试）；
   - “virtio-mem 拔出与 in-flight I/O 并发”的测试；
   - 主循环批处理与 posted notify。
4. **暂不做：**
   - 块 I/O worker 线程；
   - EVENT_IDX、INDIRECT_DESC、DISCARD；
   - 网卡 offload。

验证方式：

- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked`。
- 实机：在 `./dist/w-vmm run --disk dist/data.qcow2 --read-only` 中执行 `dd bs=4M` 应正常完成，`/sys/block/vda/queue/max_segments` 应为 126；`scripts/smoke.py` 的只读盘用例已固化这两项检查。
- 回归：运行 `scripts/smoke*.py` 冒烟脚本。
- 本轮的实际结果见开头的“最终验证”。
