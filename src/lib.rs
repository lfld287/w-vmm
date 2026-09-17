#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("w-vmm requires Apple Silicon macOS 15+");
pub mod boot;
mod devices;
pub mod net;
mod platform;
pub mod serial;
pub mod storage;
use anyhow::Result;
use net::NetDevice;
use serial::SerialIo;
use std::collections::BTreeMap;
use storage::BlockStorage;

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub memory_mib: u64,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self { memory_mib: 512 }
    }
}

pub struct Vmm {
    config: VmConfig,
}

impl Vmm {
    pub fn new(config: VmConfig) -> Self {
        Self { config }
    }

    /// Run with named disks, an optional Ethernet backend, and a serial backend.
    /// Disk names (1..20 ASCII letters, digits, '.', '_' or '-') become virtio serials.
    /// Devices are attached in name order; Linux assigns its own /dev/vd* names.
    /// Pass an empty map for no disks and a typed None for no network.
    /// Serial I/O runs on the calling thread; the caller owns terminal/signal policy.
    pub fn run<BS: BlockStorage, ND: NetDevice, SI: SerialIo>(
        self,
        blocks: BTreeMap<String, BS>,
        net: Option<ND>,
        serial: SI,
    ) -> Result<()> {
        platform::run(&self.config, blocks, net, serial)
    }
}
