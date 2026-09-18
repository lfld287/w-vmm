#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod boot;
mod control;
mod devices;
pub mod memory;
pub mod net;
pub mod platform;
mod runtime;
pub use control::{VmControl, VmLifecycle, VmStatus};
pub use memory::{MemoryLifecycle, MemoryStatus, VirtioMem};
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

/// Owns all devices from construction through shutdown.
pub struct Vmm<BS, ND, SI> {
    config: VmConfig,
    control: VmControl,
    devices: Option<Devices<BS, ND, SI>>,
}

struct Devices<BS, ND, SI> {
    blocks: BTreeMap<String, BS>,
    net: Option<ND>,
    memory: Option<VirtioMem>,
    serial: SI,
}

impl<BS: BlockStorage, ND: NetDevice, SI: SerialIo> Vmm<BS, ND, SI> {
    /// Disk names become virtio serials; devices attach in name order.
    /// Use an empty map for no disks and a typed None for no network.
    pub fn new(
        config: VmConfig,
        blocks: BTreeMap<String, BS>,
        net: Option<ND>,
        memory: Option<VirtioMem>,
        serial: SI,
    ) -> Self {
        let control = VmControl::new(memory.as_ref().map(VirtioMem::control));
        Self {
            config,
            control,
            devices: Some(Devices {
                blocks,
                net,
                memory,
                serial,
            }),
        }
    }

    /// Obtain a cloneable handle for VM and dynamic memory control.
    pub fn control(&self) -> VmControl {
        self.control.clone()
    }

    /// Run using a caller-provided platform and the owned devices.
    pub fn run_with_platform<P: platform::Platform>(mut self, platform: P) -> Result<()> {
        self.control.begin()?;
        let Devices {
            blocks,
            net,
            memory,
            serial,
        } = self.devices.take().unwrap();
        let result = runtime::run(
            &self.config,
            &self.control,
            platform,
            blocks,
            net,
            memory,
            serial,
        );
        self.control.finish(&result);
        result
    }

    /// Run with the default platform. Serial I/O stays on the calling thread.
    pub fn run(mut self) -> Result<()> {
        self.control.begin()?;
        let Devices {
            blocks,
            net,
            memory,
            serial,
        } = self.devices.take().unwrap();
        let result = platform::run(&self.config, &self.control, blocks, net, memory, serial);
        self.control.finish(&result);
        result
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
        let vm = Vmm::new(
            VmConfig {
                memory_mib: 0,
                vcpu_count: 1,
            },
            BTreeMap::<String, storage::Disk>::new(),
            None::<net::macos::Vmnet>,
            Some(memory),
            Serial,
        );
        let control = vm.control();
        let result = vm.run();
        assert!(result.is_err());
        assert_eq!(
            control.memory_status().unwrap().lifecycle,
            MemoryLifecycle::Stopped
        );
        assert!(control.set_requested_mib(2).is_err());
    }
}

impl<BS, ND, SI> Drop for Vmm<BS, ND, SI> {
    fn drop(&mut self) {
        drop(self.devices.take());
        self.control.dropped();
    }
}
