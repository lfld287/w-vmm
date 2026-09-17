//! Thin HVF binding, authored against Apple's SDK. See THIRD_PARTY.md for libkrun reference.
use crate::boot;
use anyhow::{Result, ensure};
use std::{ffi::c_void, marker::PhantomData, rc::Rc};
use vm_memory::{Address, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};

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
    ensure!(
        code == 0,
        "{op}: Hypervisor.framework error {:#x}",
        code as u32
    );
    Ok(())
}

pub fn spi(id: u32, level: bool) -> Result<()> {
    unsafe { check(hv_gic_set_spi(id, level), "GIC SPI") }
}

pub fn kick(id: u64) {
    unsafe {
        hv_vcpus_exit(&id, 1);
    }
}

// !Send and !Sync: HVF VM/vCPU operations have owning-thread requirements.
pub struct Vm {
    mem: GuestMemoryMmap,
    mapped: bool,
    _thread: PhantomData<Rc<()>>,
}

impl Vm {
    pub fn new(mem: GuestMemoryMmap) -> Result<Self> {
        unsafe {
            check(
                hv_vm_create(std::ptr::null()),
                "create VM (check ad-hoc hypervisor entitlement)",
            )?;
        }
        let mut vm = Self {
            mem,
            mapped: false,
            _thread: PhantomData,
        };
        let region = vm.mem.iter().next().unwrap();
        let ptr = region.as_ptr();
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        ensure!(
            (ptr as usize).is_multiple_of(page) && (region.len() as usize).is_multiple_of(page),
            "HVF host-page alignment"
        );
        unsafe {
            check(
                hv_vm_map(
                    ptr.cast(),
                    region.start_addr().raw_value(),
                    region.len() as usize,
                    7,
                ),
                "map RAM",
            )?;
        }
        vm.mapped = true;
        Ok(vm)
    }

    pub fn memory(&self) -> &GuestMemoryMmap {
        &self.mem
    }

    pub fn gic(&self) -> Result<(u64, [u32; 2])> {
        unsafe {
            let c = hv_gic_config_create();
            ensure!(!c.is_null(), "create GIC configuration");
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
            ensure!(
                boot::GIC_REDIST + size as u64 <= boot::RAM,
                "GIC overlaps RAM"
            );
            let mut timers = [0; 2];
            check(hv_gic_get_intid(30, &mut timers[0]), "physical timer INTID")?;
            check(hv_gic_get_intid(27, &mut timers[1]), "virtual timer INTID")?;
            Ok((size as u64, timers))
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        unsafe {
            if self.mapped {
                hv_vm_unmap(boot::RAM, self.mem.iter().next().unwrap().len() as usize);
            }
            hv_vm_destroy();
        }
    }
}

pub struct Vcpu<'a> {
    pub id: u64,
    exit: *const Exit,
    _vm: &'a Vm,
}

impl<'a> Vcpu<'a> {
    pub fn new(vm: &'a Vm, entry: u64, dtb: u64) -> Result<Self> {
        let mut id = 0;
        let mut exit = std::ptr::null();
        unsafe {
            check(
                hv_vcpu_create(&mut id, &mut exit, std::ptr::null()),
                "create vCPU",
            )?;
        }
        let v = Self { id, exit, _vm: vm };
        unsafe {
            check(hv_vcpu_set_sys_reg(id, 0xc005, 0), "MPIDR")?;
        }
        v.set(34, 0x3c5)?;
        v.set(31, entry)?;
        v.set(0, dtb)?;
        Ok(v)
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

impl Drop for Vcpu<'_> {
    fn drop(&mut self) {
        unsafe {
            hv_vcpu_destroy(self.id);
        }
    }
}
