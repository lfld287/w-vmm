//! Public memory control and platform mapping contract.
use anyhow::{Result, ensure};
use std::sync::{Arc, Mutex, MutexGuard};
use vm_memory::GuestRegionMmap;
pub(crate) const MIB: u64 = 1 << 20;
pub(crate) const HOTPLUG_BLOCK_SIZE: u64 = 128 << 20;

pub(crate) fn mib_bytes(mib: u64) -> Result<u64> {
    mib.checked_mul(MIB)
        .ok_or_else(|| anyhow::anyhow!("memory capacity overflow"))
}

/// Each operation must be atomic on error. Mappings borrow the region until
/// unmap succeeds or the VM is destroyed. Called only while vCPUs are stopped.
pub trait Mapper {
    fn map(&mut self, region: &GuestRegionMmap) -> Result<()>;

    fn unmap(&mut self, region: &GuestRegionMmap) -> Result<()>;
}

fn validate_target(target: u64, capacity: u64) -> Result<()> {
    ensure!(
        target.is_multiple_of(HOTPLUG_BLOCK_SIZE / crate::memory::MIB) && target <= capacity,
        "requested memory must be a multiple of {} MiB within region capacity",
        HOTPLUG_BLOCK_SIZE / crate::memory::MIB
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryLifecycle {
    Created,
    Running,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryStatus {
    pub region_size_mib: u64,
    pub requested_size_mib: u64,
    pub plugged_size_mib: u64,
    pub driver_ready: bool,
    pub lifecycle: MemoryLifecycle,
}

#[derive(Debug, Clone)]
pub struct MemoryControl(Arc<Mutex<MemoryStatus>>);

impl MemoryControl {
    pub(crate) fn new(region_size_mib: u64) -> Self {
        Self(Arc::new(Mutex::new(MemoryStatus {
            region_size_mib,
            requested_size_mib: 0,
            plugged_size_mib: 0,
            driver_ready: false,
            lifecycle: MemoryLifecycle::Created,
        })))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, MemoryStatus> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Accept a desired extra-memory size. The guest may not reach it immediately.
    pub fn set_requested_mib(&self, requested: u64) -> Result<()> {
        let mut state = self.lock();
        ensure!(
            state.lifecycle != MemoryLifecycle::Stopped,
            "VM has stopped"
        );
        validate_target(requested, state.region_size_mib)?;
        state.requested_size_mib = requested;
        Ok(())
    }

    pub fn status(&self) -> MemoryStatus {
        self.lock().clone()
    }
}

/// Sparse dynamic memory. Guest targets retain the existing 128 MiB constraint.
pub struct VirtioMem(pub(crate) crate::devices::memory::VirtioMem);

impl VirtioMem {
    pub fn new(region_size_mib: u64) -> Result<Self> {
        Ok(Self(crate::devices::memory::VirtioMem::new(
            region_size_mib,
        )?))
    }

    pub fn control(&self) -> MemoryControl {
        self.0.control()
    }
}
