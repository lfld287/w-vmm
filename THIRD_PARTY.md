# 第三方来源与许可

本项目的 Rust 源码使用 Apache-2.0。依赖的具体版本由 `Cargo.lock` 固定，各依赖保留自身许可。

## HVF 参考

参考 libkrun 的创建顺序、ARM64 寄存器初始化和退出 syndrome 解码：
https://github.com/libkrun/libkrun/tree/e6cdb553185c51f54c694f73cf6f98157b33e863/src/hvf

原始源码声明：

```
Copyright 2021 Red Hat, Inc.
SPDX-License-Identifier: Apache-2.0
```

`src/platform/macos_arm64/hvf.rs` 的最小 FFI 声明按本机 Apple SDK 的 Hypervisor 头文件重新编写，没有复制 libkrun 自动生成的 bindings 或完整封装。寄存器编号和 syndrome 位定义来自 ARM64/HVF ABI；VM/vCPU 生命周期采用本项目的 RAII 封装。Apple SDK 头文件未随项目重新分发。

`src/net/macos.rs` 的 vmnet/XPC 最小 FFI 同样依据本机 Apple SDK 编写，使用系统 vmnet.framework；Apple SDK 头文件不随项目分发。

## Rust 组件

- rust-vmm: vm-memory、linux-loader、vm-fdt、virtio-queue、virtio-bindings、vm-superio。
- imago 0.2.5: https://docs.rs/imago/0.2.5/imago/ ，使用官方 crate，启用 `sync` 和 `vm-memory` features，通过官方 volatile 转换器接入，内部缓冲和引用处理由 imago 管理，遵循 MIT 许可证。
- block2、dispatch2：Apple Blocks / Grand Central Dispatch 的 Rust 包装，https://github.com/madsmtm/objc2 。
- 其余依赖见 Cargo.lock 和各 crate 的 license 声明。

## 嵌入的 Alpine 资源

`assets/manifest.json` 记录官方下载链接、版本及 SHA-256。Linux 内核及模块遵循 GPL-2.0-only；BusyBox 遵循 GPL-2.0-only；Alpine minirootfs 中其他组件各自遵循其包许可。本项目的 Apache-2.0 不替代这些许可。

对应 Alpine 构建配方与源码获取入口：
- https://gitlab.alpinelinux.org/alpine/aports/-/tree/3.24-stable/main/linux-lts
- linux-virt 对应 aports commit: `09a165f4c951edf370eddde03b2d1d5fd71805ed`
- https://gitlab.alpinelinux.org/alpine/aports/-/tree/3.24-stable/main/busybox
- https://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64/

当前交付仅在本地构建与使用。若重新分发包含这些 GPL 组件的可执行文件，应一并满足对应源码及许可提供义务。
