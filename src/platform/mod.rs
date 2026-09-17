//! Per-platform VMM backends.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos_arm64;

use crate::VmConfig;
use anyhow::Result;

/// Platform VMM backend: boots the guest and runs it to completion
/// (guest poweroff/reset, Ctrl-], or host signal).
pub(crate) trait VmRuntime {
    fn run(config: &VmConfig) -> Result<()>;
}

/// Backend selected for this build target.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
type Runtime = macos_arm64::Backend;

pub(crate) fn run(config: &VmConfig) -> Result<()> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Runtime::run(config);
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    anyhow::bail!("w-vmm requires Apple Silicon macOS 15+"); // unreachable: crate-root compile_error! fires first
}
