//! Per-platform VMM backends.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos_arm64;

use crate::{VmConfig, net::NetDevice, serial::SerialIo, storage::BlockStorage};
use anyhow::Result;
use std::collections::BTreeMap;

/// Platform VMM backend: boots the guest and runs it to completion
/// (guest poweroff/reset or a serial backend stop request).
pub(crate) trait VmRuntime {
    fn run<BS: BlockStorage, ND: NetDevice, SI: SerialIo>(
        config: &VmConfig,
        control: Option<crate::MemoryControl>,
        blocks: BTreeMap<String, BS>,
        net: Option<ND>,
        serial: SI,
    ) -> Result<()>;
}

/// Backend selected for this build target.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
type Runtime = macos_arm64::Backend;

pub(crate) fn run<BS: BlockStorage, ND: NetDevice, SI: SerialIo>(
    config: &VmConfig,
    control: Option<crate::MemoryControl>,
    blocks: BTreeMap<String, BS>,
    net: Option<ND>,
    serial: SI,
) -> Result<()> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Runtime::run(config, control, blocks, net, serial);
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    anyhow::bail!("w-vmm requires Apple Silicon macOS 15+"); // unreachable: crate-root compile_error! fires first
}
