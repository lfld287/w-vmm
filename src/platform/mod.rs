//! Implement a platform to reuse the device models and runtime on Linux or macOS.
use crate::{VmConfig, memory::Mapper};
use anyhow::{Result, ensure};
use std::time::Duration;
use vm_memory::GuestMemoryMmap;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos_arm64;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use macos_arm64::Hvf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoSpace {
    Mmio,
    Port,
}

#[derive(Clone, Copy, Debug)]
pub struct MemoryRange {
    pub address: u64,
    pub size: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct IoRegion {
    pub space: IoSpace,
    pub address: u64,
    pub size: u64,
    pub irq: u32,
}

#[derive(Clone, Debug)]
pub enum DeviceKind {
    Block(String),
    Net,
    Memory,
}

#[derive(Clone, Debug)]
pub struct MemoryRequirement {
    pub capacity: u64,
    pub alignment: u64,
}

#[derive(Clone, Debug)]
pub struct DeviceRequirements {
    /// Sorted disks, followed by network and dynamic memory.
    pub devices: Vec<DeviceKind>,
    pub hotplug: Option<MemoryRequirement>,
}

#[derive(Clone, Debug)]
pub struct MachineLayout {
    pub ram: Vec<MemoryRange>,
    pub uart: IoRegion,
    pub virtio: Vec<IoRegion>,
    pub hotplug: Option<MemoryRange>,
}

impl MachineLayout {
    pub(crate) fn validate(&self, config: &VmConfig, needs: &DeviceRequirements) -> Result<()> {
        ensure!(config.vcpu_count > 0, "vCPU count must be positive");
        ensure!(!self.ram.is_empty(), "no base RAM");
        ensure!(
            self.virtio.len() == needs.devices.len(),
            "device count mismatch"
        );
        ensure!(self.uart.size >= 8, "UART region too small");
        let mut ranges = Vec::new();
        let mut total = 0u64;
        let mut add = |space, address: u64, size: u64| -> Result<()> {
            ensure!(size > 0, "empty region");
            let end = address
                .checked_add(size)
                .ok_or_else(|| anyhow::anyhow!("region overflow"))?;
            if space == IoSpace::Port {
                ensure!(end <= 65536, "port range overflow");
            }
            ensure!(
                !ranges
                    .iter()
                    .any(|&(s, a, e)| s == space && address < e && a < end),
                "overlapping regions"
            );
            ranges.push((space, address, end));
            Ok(())
        };
        for r in &self.ram {
            usize::try_from(r.size)?;
            total = total
                .checked_add(r.size)
                .ok_or_else(|| anyhow::anyhow!("RAM capacity overflow"))?;
            add(IoSpace::Mmio, r.address, r.size)?;
        }
        ensure!(
            total == crate::memory::mib_bytes(config.memory_mib)?,
            "RAM capacity mismatch"
        );
        add(self.uart.space, self.uart.address, self.uart.size)?;
        for r in &self.virtio {
            ensure!(
                r.space == IoSpace::Mmio && r.size >= 0x200,
                "invalid virtio MMIO region"
            );
            add(r.space, r.address, r.size)?;
        }
        match (&self.hotplug, &needs.hotplug) {
            (Some(r), Some(n)) => {
                ensure!(
                    r.size == n.capacity && r.address.is_multiple_of(n.alignment),
                    "invalid hotplug capacity/alignment"
                );
                add(IoSpace::Mmio, r.address, r.size)?;
            }
            (None, None) => (),
            _ => anyhow::bail!("hotplug layout mismatch"),
        }
        Ok(())
    }
}

/// A platform owns architecture-specific layout and VM creation policy.
pub trait Platform {
    type Vm: VirtualMachine;

    fn layout(&self, config: &VmConfig, devices: &DeviceRequirements) -> Result<MachineLayout>;

    fn create(&self, config: &VmConfig) -> Result<Self::Vm>;
}

/// Values are little-endian; width is in bytes. Completion is opaque to the runtime.
pub struct IoAccess<C> {
    pub space: IoSpace,
    pub address: u64,
    pub width: usize,
    pub write: bool,
    pub value: u64,
    pub completion: C,
}

pub enum Event<C> {
    Io(IoAccess<C>),
    Shutdown,
}

/// VM operations run on the caller's thread; no Send/Sync bound is required.
/// `prepare` loads the guest and initializes CPUs/interrupts without running them.
/// `pause` must quiesce every vCPU before returning success. `stop` must always
/// join every worker, including after partial startup, and be idempotent. Drop
/// must also stop workers and destroy all mappings without releasing borrowed RAM.
pub trait VirtualMachine: Mapper {
    type Completion;

    fn prepare(&mut self, memory: &GuestMemoryMmap, layout: &MachineLayout) -> Result<()>;

    fn start(&mut self) -> Result<()>;

    fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event<Self::Completion>>>;

    fn complete_io(&mut self, completion: Self::Completion, value: u64) -> Result<()>;

    fn set_irq(&mut self, irq: u32, level: bool) -> Result<()>;

    fn pause(&mut self) -> Result<()>;

    fn resume(&mut self) -> Result<()>;

    fn stop(&mut self);
}

pub(crate) fn run<
    BS: crate::storage::BlockStorage,
    ND: crate::net::NetDevice,
    SI: crate::serial::SerialIo,
>(
    config: &VmConfig,
    blocks: std::collections::BTreeMap<String, BS>,
    net: Option<ND>,
    memory: Option<crate::VirtioMem>,
    serial: SI,
) -> Result<()> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        crate::runtime::run(config, Hvf, blocks, net, memory, serial)
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = (config, blocks, net, memory, serial);
        anyhow::bail!("no default platform; use Vmm::run_with_platform")
    }
}
