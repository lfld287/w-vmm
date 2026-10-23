//! Thin HVF binding, authored against Apple's SDK. See THIRD_PARTY.md for libkrun reference.
use crate::error::PlatformError;
use crate::{boot, memory::Mapper};
use std::{collections::BTreeMap, ffi::c_void, marker::PhantomData, rc::Rc, sync::Arc};
use vm_memory::{Address, GuestMemoryRegion, GuestRegionMmap};

type Result<T> = std::result::Result<T, PlatformError>;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Exception {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Exit {
    pub reason: u32,
    pub exception: Exception,
}

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_get_max_vcpu_count(count: *mut u32) -> i32;

    fn hv_vm_config_get_default_ipa_size(bits: *mut u32) -> i32;

    fn hv_vm_create(config: *const c_void) -> i32;

    fn hv_vm_destroy() -> i32;

    fn hv_vm_map(addr: *const c_void, ipa: u64, size: usize, flags: u64) -> i32;

    fn hv_vm_unmap(ipa: u64, size: usize) -> i32;

    fn hv_gic_config_create() -> *mut c_void;

    fn hv_gic_config_set_distributor_base(c: *mut c_void, a: u64) -> i32;

    fn hv_gic_config_set_redistributor_base(c: *mut c_void, a: u64) -> i32;

    fn hv_gic_create(c: *mut c_void) -> i32;

    fn hv_gic_get_redistributor_region_size(size: *mut usize) -> i32;

    fn hv_gic_get_intid(kind: u16, id: *mut u32) -> i32;

    fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> i32;

    fn hv_gic_set_spi(id: u32, level: bool) -> i32;

    fn hv_vcpu_create(id: *mut u64, exit: *mut *const Exit, c: *const c_void) -> i32;

    fn hv_vcpu_destroy(id: u64) -> i32;

    fn hv_vcpu_run(id: u64) -> i32;

    fn hv_vcpus_exit(ids: *const u64, count: u32) -> i32;

    fn hv_vcpu_get_reg(id: u64, reg: u32, value: *mut u64) -> i32;

    fn hv_vcpu_set_reg(id: u64, reg: u32, value: u64) -> i32;

    fn hv_vcpu_set_sys_reg(id: u64, reg: u16, value: u64) -> i32;

    fn os_release(obj: *mut c_void);
}

fn check(code: i32, op: &str) -> Result<()> {
    if code != 0 {
        return Err(PlatformError::Hypervisor {
            operation: op.to_owned(),
            code,
        });
    }
    Ok(())
}

pub fn spi(id: u32, level: bool) -> Result<()> {
    unsafe { check(hv_gic_set_spi(id, level), "GIC SPI") }
}

pub fn validate_irqs(regions: &[boot::VirtioRegion]) -> Result<()> {
    let (mut base, mut count) = (0, 0);
    unsafe {
        check(
            hv_gic_get_spi_interrupt_range(&mut base, &mut count),
            "GIC SPI range",
        )?;
    }
    let end = base
        .checked_add(count)
        .ok_or_else(|| PlatformError::InvalidGicSpiRange)?;
    if !((base..end).contains(&boot::UART_IRQ)
        && regions.iter().all(|r| (base..end).contains(&r.irq)))
    {
        return Err(PlatformError::GicSpiRange { base, end });
    }
    Ok(())
}

pub fn kick(id: u64) {
    unsafe {
        hv_vcpus_exit(&id, 1);
    }
}

// !Send and !Sync: HVF VM/vCPU operations have owning-thread requirements.
pub struct Vm {
    mappings: Mappings,
    _thread: PhantomData<Rc<()>>,
}

/// Regions mapped into HVF, keyed by guest address. HVF only receives host
/// pointers, so the table owns each allocation until a successful unmap or VM
/// destruction.
#[derive(Default)]
struct Mappings(BTreeMap<u64, Arc<GuestRegionMmap>>);

impl Mappings {
    fn map(
        &mut self,
        region: Arc<GuestRegionMmap>,
        map: impl FnOnce(&GuestRegionMmap) -> Result<()>,
    ) -> Result<()> {
        let (address, size) = (region.start_addr().raw_value(), region.len());
        let end = address
            .checked_add(size)
            .ok_or_else(|| PlatformError::RegionOverflow { address, size })?;
        if self
            .0
            .range(..end)
            .next_back()
            .is_some_and(|(&a, r)| a + r.len() > address)
        {
            return Err(PlatformError::MappingOverlap { address, size });
        }
        map(&region)?;
        self.0.insert(address, region);
        Ok(())
    }

    /// The allocation is released only after `unmap` succeeds.
    fn unmap(
        &mut self,
        region: &GuestRegionMmap,
        unmap: impl FnOnce(u64, u64) -> Result<()>,
    ) -> Result<()> {
        let (address, size) = (region.start_addr().raw_value(), region.len());
        let mapped = self
            .0
            .get(&address)
            .filter(|r| r.len() == size)
            .ok_or_else(|| PlatformError::MappingNotFound { address, size })?;
        // Another wrapper around the same allocation is the same mapping.
        if !Arc::ptr_eq(&mapped.get_mmap(), &region.get_mmap()) {
            return Err(PlatformError::MappingMismatch { address, size });
        }
        unmap(address, size)?;
        self.0.remove(&address);
        Ok(())
    }

    /// Without a successful VM destruction, HVF may still reference every
    /// mapped allocation, so they are leaked rather than released.
    fn destroyed(&mut self, success: bool) {
        let mappings = std::mem::take(&mut self.0);
        if !success {
            std::mem::forget(mappings);
        }
    }
}

impl Vm {
    pub fn new() -> Result<Self> {
        unsafe {
            check(
                hv_vm_create(std::ptr::null()),
                "create VM (check hypervisor entitlement)",
            )?;
        }
        Ok(Self {
            mappings: Mappings::default(),
            _thread: PhantomData,
        })
    }

    pub fn gic(&self) -> Result<(u64, [u32; 2])> {
        unsafe {
            let c = hv_gic_config_create();
            if c.is_null() {
                return Err(PlatformError::CreateGicConfiguration);
            }
            let result = (|| {
                check(
                    hv_gic_config_set_distributor_base(c, boot::GIC_DIST),
                    "GIC distributor",
                )?;
                check(
                    hv_gic_config_set_redistributor_base(c, boot::GIC_REDIST),
                    "GIC redistributor",
                )?;
                check(hv_gic_create(c), "create GIC")
            })();
            os_release(c);
            result?;
            let mut size = 0;
            check(hv_gic_get_redistributor_region_size(&mut size), "GIC size")?;
            if boot::GIC_REDIST + size as u64 > boot::RAM {
                return Err(PlatformError::GicOverlapsRam);
            }
            let mut timers = [0; 2];
            check(hv_gic_get_intid(30, &mut timers[0]), "physical timer INTID")?;
            check(hv_gic_get_intid(27, &mut timers[1]), "virtual timer INTID")?;
            Ok((size as u64, timers))
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let result = unsafe { check(hv_vm_destroy(), "destroy VM") };
        // Secure the leak before logging: a panicking eprintln! must not free
        // allocations Hypervisor.framework may still map.
        self.mappings.destroyed(result.is_ok());
        if let Err(e) = &result {
            eprintln!("{}; leaking mapped guest RAM", crate::error::diagnostic(e));
        }
    }
}

// Vm::new passes NULL configuration, so the default is this VM's actual width.
pub fn ipa_bits() -> Result<u32> {
    let mut bits = 0;
    unsafe {
        check(
            hv_vm_config_get_default_ipa_size(&mut bits),
            "default VM IPA size",
        )?;
    }
    Ok(bits)
}

pub fn max_vcpus() -> Result<u32> {
    let mut count = 0;
    unsafe {
        check(hv_vm_get_max_vcpu_count(&mut count), "max vCPU count")?;
    }
    Ok(count)
}

impl Mapper for Vm {
    fn map(
        &mut self,
        region: Arc<GuestRegionMmap>,
    ) -> std::result::Result<(), crate::error::MemoryError> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        if !((region.as_ptr() as u64).is_multiple_of(page)
            && region.len().is_multiple_of(page)
            && region.start_addr().raw_value().is_multiple_of(page))
        {
            return Err(PlatformError::HvfHostPageAlignment.into());
        }
        boot::validate_ipa_range(region.start_addr().raw_value(), region.len(), ipa_bits()?)
            .map_err(PlatformError::from)?;
        Ok(self.mappings.map(region, |region| unsafe {
            // The table keeps the allocation alive while HVF references it.
            check(
                hv_vm_map(
                    region.as_ptr().cast(),
                    region.start_addr().raw_value(),
                    region.len() as usize,
                    7,
                ),
                "map guest RAM",
            )
        })?)
    }

    fn unmap(
        &mut self,
        region: &GuestRegionMmap,
    ) -> std::result::Result<(), crate::error::MemoryError> {
        Ok(self.mappings.unmap(region, |address, size| unsafe {
            check(hv_vm_unmap(address, size as usize), "unmap guest RAM")
        })?)
    }
}

// Created and destroyed on its worker thread. Cpus is joined before Vm drops.
pub struct Vcpu {
    pub id: u64,
    exit: *const Exit,
    _thread: PhantomData<Rc<()>>,
}

impl Vcpu {
    pub fn new(mpidr: u64) -> Result<Self> {
        let mut id = 0;
        let mut exit = std::ptr::null();
        unsafe {
            check(
                hv_vcpu_create(&mut id, &mut exit, std::ptr::null()),
                "create vCPU",
            )?;
        }
        let v = Self {
            id,
            exit,
            _thread: PhantomData,
        };
        unsafe {
            check(hv_vcpu_set_sys_reg(id, 0xc005, mpidr), "MPIDR")?;
        }
        Ok(v)
    }

    pub fn boot(&self, entry: u64, context: u64) -> Result<()> {
        // CPU_ON re-enters at EL1 with translation and caches disabled.
        unsafe {
            check(
                hv_vcpu_set_sys_reg(self.id, 0xc080, 0x30d00800),
                "SCTLR_EL1",
            )?;
        }
        for reg in 0..31 {
            self.set(reg, 0)?;
        }
        self.set(34, 0x3c5)?;
        self.set(31, entry)?;
        self.set(0, context)
    }

    pub fn run(&self) -> Result<Exit> {
        unsafe {
            check(hv_vcpu_run(self.id), "run vCPU")?;
            Ok(*self.exit)
        }
    }

    pub fn get(&self, reg: u32) -> Result<u64> {
        let mut v = 0;
        unsafe {
            check(hv_vcpu_get_reg(self.id, reg, &mut v), "read register")?;
        }
        Ok(v)
    }

    pub fn set(&self, reg: u32, v: u64) -> Result<()> {
        unsafe { check(hv_vcpu_set_reg(self.id, reg, v), "write register") }
    }

    pub fn advance(&self) -> Result<()> {
        self.set(31, self.get(31)? + 4)
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        unsafe {
            hv_vcpu_destroy(self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestAddress;

    fn region(address: u64, size: usize) -> Arc<GuestRegionMmap> {
        Arc::new(GuestRegionMmap::from_range(GuestAddress(address), size, None).unwrap())
    }

    fn fail() -> Result<()> {
        Err(PlatformError::Backend(
            std::io::Error::other("injected").into(),
        ))
    }

    #[test]
    fn table_owns_mappings_until_successful_unmap() {
        let mut m = Mappings::default();
        let a = region(0x10000, 0x4000);
        let weak = Arc::downgrade(&a.get_mmap());
        m.map(a.clone(), |_| Ok(())).unwrap();
        drop(a);
        assert!(weak.upgrade().is_some());
        let a = m.0[&0x10000].clone();
        // Address, length and allocation identity must all match.
        let other = region(0x10000, 0x4000);
        assert!(matches!(
            m.unmap(&other, |_, _| unreachable!()),
            Err(PlatformError::MappingMismatch { .. })
        ));
        let moved = GuestRegionMmap::with_arc(a.get_mmap(), GuestAddress(0x14000)).unwrap();
        assert!(matches!(
            m.unmap(&moved, |_, _| unreachable!()),
            Err(PlatformError::MappingNotFound { .. })
        ));
        drop(moved);
        m.map(region(0x20000, 0x8000), |_| Ok(())).unwrap();
        let resized =
            GuestRegionMmap::with_arc(region(0, 0x4000).get_mmap(), GuestAddress(0x20000)).unwrap();
        assert!(matches!(
            m.unmap(&resized, |_, _| unreachable!()),
            Err(PlatformError::MappingNotFound { .. })
        ));
        // A failed unmap keeps ownership; the same region may be retried.
        assert!(m.unmap(&a, |_, _| fail()).is_err());
        drop(a);
        assert!(weak.upgrade().is_some());
        let wrapper =
            GuestRegionMmap::with_arc(m.0[&0x10000].get_mmap(), GuestAddress(0x10000)).unwrap();
        m.unmap(&wrapper, |address, size| {
            assert_eq!((address, size), (0x10000, 0x4000));
            Ok(())
        })
        .unwrap();
        drop(wrapper);
        assert!(weak.upgrade().is_none());
        assert_eq!(m.0.len(), 1);
    }

    #[test]
    fn overlap_and_failed_map_do_not_take_ownership() {
        let mut m = Mappings::default();
        m.map(region(0x10000, 0x4000), |_| Ok(())).unwrap();
        for (address, size) in [(0x10000, 0x4000), (0xf000, 0x2000), (0x13000, 0x4000)] {
            assert!(matches!(
                m.map(region(address, size), |_| unreachable!()),
                Err(PlatformError::MappingOverlap { .. })
            ));
        }
        m.map(region(0xc000, 0x4000), |_| Ok(())).unwrap();
        m.map(region(0x14000, 0x4000), |_| Ok(())).unwrap();
        let failed = region(0x20000, 0x4000);
        let weak = Arc::downgrade(&failed.get_mmap());
        assert!(m.map(failed, |_| fail()).is_err());
        assert!(weak.upgrade().is_none());
        assert_eq!(m.0.len(), 3);
    }

    #[test]
    fn destroy_failure_leaks_mapped_allocations() {
        for success in [true, false] {
            let mut m = Mappings::default();
            let r = region(0x10000, 0x4000);
            let weak = Arc::downgrade(&r.get_mmap());
            m.map(r, |_| Ok(())).unwrap();
            m.destroyed(success);
            drop(m);
            assert_eq!(weak.upgrade().is_none(), success);
        }
    }
}
