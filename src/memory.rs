//! Thread-safe desired-memory control. Targets are asynchronous guest requests.
use anyhow::{Result, ensure};
use std::sync::{Arc, Mutex, MutexGuard};

pub(crate) const BLOCK: u64 = 2 << 20;
pub(crate) const ALIGN: u64 = 128 << 20;

#[derive(Debug, Clone)]
pub struct VirtioMemConfig {
    pub region_size_mib: u64,
    pub requested_size_mib: u64,
}
impl VirtioMemConfig {
    pub(crate) fn validate(&self, base_mib: u64) -> Result<()> {
        ensure!(
            self.region_size_mib > 0 && self.region_size_mib.is_multiple_of(128),
            "virtio-mem region must be a positive multiple of 128 MiB"
        );
        ensure!(
            base_mib
                .checked_add(self.region_size_mib)
                .is_some_and(|n| n <= 16384),
            "total RAM capacity exceeds 16384 MiB"
        );
        validate_target(self.requested_size_mib, self.region_size_mib)
    }
}
fn validate_target(target: u64, capacity: u64) -> Result<()> {
    ensure!(
        target.is_multiple_of(2) && target <= capacity,
        "requested memory must be a multiple of 2 MiB within region capacity"
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
    pub(crate) fn new(config: &VirtioMemConfig) -> Self {
        Self(Arc::new(Mutex::new(MemoryStatus {
            region_size_mib: config.region_size_mib,
            requested_size_mib: config.requested_size_mib,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn control_lifecycle_and_concurrency() {
        let c = MemoryControl::new(&VirtioMemConfig {
            region_size_mib: 128,
            requested_size_mib: 0,
        });
        assert!(!c.status().driver_ready);
        assert!(c.set_requested_mib(3).is_err());
        assert!(c.set_requested_mib(130).is_err());
        std::thread::scope(|s| {
            for n in 0..32 {
                let c = c.clone();
                s.spawn(move || c.set_requested_mib(n * 2).unwrap());
            }
        });
        c.lock().lifecycle = MemoryLifecycle::Running;
        c.set_requested_mib(128).unwrap();
        c.lock().lifecycle = MemoryLifecycle::Stopped;
        assert!(c.set_requested_mib(0).is_err());
        assert_eq!(c.status().requested_size_mib, 128);
    }
    #[test]
    fn capacity_validation() {
        for (region, requested, ok) in [
            (128, 0, true),
            (0, 0, false),
            (129, 0, false),
            (128, 1, false),
            (128, 130, false),
            (u64::MAX, 0, false),
            (16384, 0, false),
        ] {
            assert_eq!(
                VirtioMemConfig {
                    region_size_mib: region,
                    requested_size_mib: requested
                }
                .validate(512)
                .is_ok(),
                ok
            );
        }
    }
}
