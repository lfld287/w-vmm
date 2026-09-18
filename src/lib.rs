#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod boot;
mod devices;
pub mod memory;
pub mod net;
pub mod platform;
mod runtime;
pub use memory::{MemoryControl, MemoryLifecycle, MemoryStatus, VirtioMem};
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
    pub vcpu_count: u32,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            memory_mib: 512,
            vcpu_count: 1,
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

    /// Run using a caller-provided platform and the built-in devices.
    pub fn run_with_platform<
        P: platform::Platform,
        BS: BlockStorage,
        ND: NetDevice,
        SI: SerialIo,
    >(
        self,
        platform: P,
        blocks: BTreeMap<String, BS>,
        net: Option<ND>,
        memory: Option<VirtioMem>,
        serial: SI,
    ) -> Result<()> {
        runtime::run(&self.config, platform, blocks, net, memory, serial)
    }

    /// Run with named disks, optional Ethernet and memory devices, and a serial backend.
    /// Disk names (1..20 ASCII letters, digits, '.', '_' or '-') become virtio serials.
    /// Devices are attached in name order; Linux assigns its own /dev/vd* names.
    /// Pass an empty map for no disks and a typed None for no network.
    /// Serial I/O runs on the calling thread; the caller owns terminal/signal policy.
    pub fn run<BS: BlockStorage, ND: NetDevice, SI: SerialIo>(
        self,
        blocks: BTreeMap<String, BS>,
        net: Option<ND>,
        memory: Option<VirtioMem>,
        serial: SI,
    ) -> Result<()> {
        platform::run(&self.config, blocks, net, memory, serial)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    struct Serial;

    impl std::io::Write for Serial {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            unreachable!()
        }

        fn flush(&mut self) -> std::io::Result<()> {
            unreachable!()
        }
    }

    impl SerialIo for Serial {
        fn recv(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            unreachable!()
        }
    }

    #[test]
    fn startup_failure_stops_memory_device() {
        let memory = VirtioMem::new(128).unwrap();
        let control = memory.control();
        let result = Vmm::new(VmConfig {
            memory_mib: 0,
            vcpu_count: 1,
        })
        .run(
            BTreeMap::<String, storage::Disk>::new(),
            None::<net::macos::Vmnet>,
            Some(memory),
            Serial,
        );
        assert!(result.is_err());
        assert_eq!(control.status().lifecycle, MemoryLifecycle::Stopped);
        assert!(control.set_requested_mib(2).is_err());
    }
}
