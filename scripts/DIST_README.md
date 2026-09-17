# w-vmm 本地运行包

Apple Silicon / macOS 15+。整个目录可以移动，无需安装 Rust、Python、Docker 或 QEMU；内核和 initramfs 已嵌入可执行文件。

```sh
./run.sh
```

默认使用旁边的 `data.qcow2`。进入 Alpine shell 后：

```sh
mount /dev/vda /data
echo hello > /data/hello.txt
sync
umount /data
poweroff
```

其他启动方式：

```sh
./w-vmm run                              # 无数据盘
./w-vmm run --disk data.qcow2 --read-only # 只读数据盘
./run.sh --disk data.qcow2 --memory-mib 1024
./w-vmm run --disk sda=data.qcow2 --disk sdb=extra.qcow2
sudo ./w-vmm run --net --disk sda=data.qcow2
```

`Ctrl-]` 退出宿主，`Ctrl-C` 发送给客户机。数据盘内容持久保存，客户机根目录的修改在退出后丢失。重新执行源码目录的 `build.sh` 会更新程序和说明，保留已有的 `dist/data.qcow2`。

文件说明：`w-vmm` 是已本地签名的可执行文件，`run.sh` 是启动入口，`data.qcow2` 是 ext4 数据盘，`manifest.json` 是嵌入资源的版本/校验清单。许可和第三方来源见 `LICENSE`、`THIRD_PARTY.md`。

磁盘按注册名排序接入，名称可通过客户机 `/sys/block/vda/serial` 等读取；Linux 自行分配 `/dev/vda`、`/dev/vdb` 等名称。`--read-only` 作用于所有指定磁盘。

网络使用系统 vmnet NAT，需要 root 或 Apple 批准的网络 entitlement。默认无网卡；开启后不自动配置 IP、路由或运行 DHCP 客户端。启动时打印后端返回的 MAC、MTU 和 IPv4 子网信息，客户机按需手动配置。

## 多核和动态内存

```sh
./w-vmm run --vcpus 4 --memory-mib 512 --virtio-mem-size-mib 1024 \
  --control-socket /tmp/w-vmm.sock
# 另一个本地终端
./w-vmm memory-set --socket /tmp/w-vmm.sock --requested-mib 768
./w-vmm memory-status --socket /tmp/w-vmm.sock
```

默认仍为 1 核、512 MiB 基础内存、不启用动态内存。核数受宿主 HVF 上限约束。基础 RAM 不可移除；动态区域是额外内存，区域容量为正的 128 MiB 倍数，目标为 2 MiB 倍数，基础加区域容量最多 16384 MiB。

控制命令输出 JSON。目标接受后由客户机异步扩缩容；`plugged_size_mib` 表示实际插入量，可能暂时无法达到目标。socket 权限为 `0600`，拒绝覆盖已有路径，仅退出时清理自己创建的 socket。CPU 数量启动后固定，支持客户机次级核离线和重新上线。

## 网络测试

在 Mac 终端启动（不需要磁盘）：

```sh
sudo ./w-vmm run --net
```

**先读取启动日志中的 `gateway` 和 `subnet_mask`，不要直接照搬示例子网。** 如果实际输出为网关 `192.168.2.1`、掩码 `255.255.255.0`，且已确认 `192.168.2.10` 空闲，在客户机 shell 执行：

```sh
ip -4 addr flush dev eth0
ip link set eth0 up
ip addr add 192.168.2.10/24 dev eth0
ip route replace default via 192.168.2.1 dev eth0
ping -c 3 192.168.2.1
```

网关能回复后，再用 `ping -c 3 1.1.1.1` 测试 NAT 出网。网关或掩码不同时，先替换命令中的地址及前缀；客户机地址为临时手工配置，需避免与其他设备冲突。`ip route get 1.1.1.1` 只显示路由，不证明网关可达。网关不通时查看 `ip neigh show dev eth0`，优先检查子网与邻居解析。

测试后执行 `poweroff`。自动化大包、TCP 上传下载及多盘共存测试见源码仓库 `scripts/smoke-net.py` 和根目录 README 的“vmnet 自动化测试”。
