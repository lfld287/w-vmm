//! Public memory control and platform mapping contract.
use crate::error::MemoryError;
use std::sync::{Arc, Mutex, MutexGuard};
use vm_memory::GuestRegionMmap;

type Result<T> = std::result::Result<T, MemoryError>;
pub(crate) const MIB: u64 = 1 << 20;
pub(crate) const HOTPLUG_BLOCK_SIZE: u64 = 128 << 20;

pub(crate) fn mib_bytes(mib: u64) -> Result<u64> {
    mib.checked_mul(MIB)
        .ok_or_else(|| MemoryError::CapacityOverflow { mib })
}

/// Each operation must be atomic on error. Called only while vCPUs are stopped.
/// A successful `map` transfers ownership of the region to the mapper, which
/// must keep it alive until a successful `unmap` or VM destruction; callers may
/// drop their references at any time. `unmap` must reject a region whose guest
/// address, length or host allocation differs from the mapping, and keep the
/// allocation when unmapping fails.
pub trait Mapper {
    fn map(&mut self, region: Arc<GuestRegionMmap>) -> Result<()>;

    fn unmap(&mut self, region: &GuestRegionMmap) -> Result<()>;
}

fn validate_target(target: u64, capacity: u64, block_size_mib: u64) -> Result<()> {
    if !(target.is_multiple_of(block_size_mib) && target <= capacity) {
        return Err(MemoryError::InvalidTarget {
            requested: target,
            capacity,
            block_size_mib,
        });
    }
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
    pub block_size_mib: u64,
    pub requested_size_mib: u64,
    pub plugged_size_mib: u64,
    pub driver_ready: bool,
    pub lifecycle: MemoryLifecycle,
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryControl(Arc<Mutex<MemoryStatus>>);

impl MemoryControl {
    pub(crate) fn new(region_size_mib: u64, block_size_mib: u64) -> Self {
        Self(Arc::new(Mutex::new(MemoryStatus {
            region_size_mib,
            block_size_mib,
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
        if state.lifecycle == MemoryLifecycle::Stopped {
            return Err(MemoryError::Stopped);
        }
        validate_target(requested, state.region_size_mib, state.block_size_mib)?;
        state.requested_size_mib = requested;
        Ok(())
    }

    pub fn status(&self) -> MemoryStatus {
        self.lock().clone()
    }
}

/// Sparse dynamic memory with an immutable device block size.
/// Targets are block-aligned; guest adjustment may use a coarser granularity.
pub struct VirtioMem(pub(crate) crate::devices::memory::VirtioMem);

impl VirtioMem {
    pub fn new(region_size_mib: u64, block_size_mib: u64) -> Result<Self> {
        Ok(Self(crate::devices::memory::VirtioMem::new(
            region_size_mib,
            block_size_mib,
        )?))
    }

    pub(crate) fn control(&self) -> MemoryControl {
        self.0.control()
    }
}
