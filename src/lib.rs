#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("w-vmm requires Apple Silicon macOS 15+");
pub mod boot;
mod devices;
mod platform;
pub mod storage;
mod terminal;
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub disk: Option<PathBuf>,
    pub memory_mib: u64,
    pub read_only: bool,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            disk: None,
            memory_mib: 512,
            read_only: false,
        }
    }
}

pub struct Vmm {
    config: VmConfig,
}

impl Vmm {
    pub fn new(config: VmConfig) -> Self {
        Self { config }
    }

    pub fn run(self) -> Result<()> {
        platform::run(&self.config)
    }
}
