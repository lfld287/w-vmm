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
```

`Ctrl-]` 退出宿主，`Ctrl-C` 发送给客户机。数据盘内容持久保存，客户机根目录的修改在退出后丢失。重新执行源码目录的 `build.sh` 会更新程序和说明，保留已有的 `dist/data.qcow2`。

文件说明：`w-vmm` 是已本地签名的可执行文件，`run.sh` 是启动入口，`data.qcow2` 是 ext4 数据盘，`manifest.json` 是嵌入资源的版本/校验清单。许可和第三方来源见 `LICENSE`、`THIRD_PARTY.md`。
