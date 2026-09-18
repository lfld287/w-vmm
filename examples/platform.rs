//! A complete in-process platform example. No hypervisor or guest image needed.
//! Run: cargo run -p w-vmm --example platform
use std::{
    collections::BTreeMap,
    io::{self, Write},
    time::Duration,
};
use vm_memory::{GuestMemoryMmap, GuestMemoryRegion, GuestRegionMmap};
use w_vmm::Result;
use w_vmm::error::{MemoryError, NetError, PlatformError};
use w_vmm::{
    VmConfig, Vmm, memory::Mapper, net::NetDevice, platform::*, serial::SerialIo, storage::Disk,
};

struct InProcess;

struct Vm {
    mappings: BTreeMap<u64, u64>,
    running: bool,
    next: usize,
}

impl Platform for InProcess {
    type Vm = Vm;

    fn layout(
        &self,
        config: &VmConfig,
        needs: &DeviceRequirements,
    ) -> std::result::Result<MachineLayout, PlatformError> {
        if !(needs.devices.is_empty() && needs.hotplug.is_none()) {
            return Err(PlatformError::Backend(
                io::Error::other("example supports only UART").into(),
            ));
        }
        let size = config
            .memory_mib
            .checked_mul(1 << 20)
            .ok_or_else(|| PlatformError::Backend(io::Error::other("RAM overflow").into()))?;
        Ok(MachineLayout {
            ram: vec![MemoryRange {
                address: 0x100000,
                size,
            }],
            uart: IoRegion {
                space: IoSpace::Port,
                address: 0x3f8,
                size: 8,
                irq: 4,
            },
            virtio: vec![],
            hotplug: None,
        })
    }

    fn create(&self, config: &VmConfig) -> std::result::Result<Vm, PlatformError> {
        if config.vcpu_count != 1 {
            return Err(PlatformError::Backend(
                io::Error::other("example supports one CPU").into(),
            ));
        }
        Ok(Vm {
            mappings: BTreeMap::new(),
            running: false,
            next: 0,
        })
    }
}

impl Mapper for Vm {
    fn map(&mut self, region: &GuestRegionMmap) -> std::result::Result<(), MemoryError> {
        if self.running {
            return Err(MemoryError::Backend(
                io::Error::other("mapping requires quiescence").into(),
            ));
        }
        let address = region.start_addr().0;
        if self.mappings.contains_key(&address) {
            return Err(MemoryError::Backend(
                io::Error::other("already mapped").into(),
            ));
        }
        // A real hypervisor maps region.as_ptr() here, borrowing its allocation.
        self.mappings.insert(address, region.len());
        Ok(())
    }

    fn unmap(&mut self, region: &GuestRegionMmap) -> std::result::Result<(), MemoryError> {
        if self.running {
            return Err(MemoryError::Backend(
                io::Error::other("unmapping requires quiescence").into(),
            ));
        }
        self.mappings.remove(&region.start_addr().0);
        Ok(())
    }
}

impl VirtualMachine for Vm {
    type Completion = usize;

    fn prepare(
        &mut self,
        _: &GuestMemoryMmap,
        _: &MachineLayout,
    ) -> std::result::Result<(), PlatformError> {
        // Real platforms load their image, initialize registers and interrupt
        // controllers, and create parked vCPUs here. No CPU may execute yet.
        Ok(())
    }

    fn start(&mut self) -> std::result::Result<(), PlatformError> {
        self.running = true;
        Ok(())
    }

    fn poll_event(
        &mut self,
        _: Duration,
    ) -> std::result::Result<Option<Event<usize>>, PlatformError> {
        let message = b"Hello from a custom platform!\n";
        if self.next == message.len() {
            return Ok(Some(Event::Shutdown));
        }
        Ok(Some(Event::Io(IoAccess {
            space: IoSpace::Port,
            address: 0x3f8,
            width: 1,
            write: true,
            value: message[self.next] as u64,
            completion: self.next,
        })))
    }

    fn complete_io(&mut self, completion: usize, _: u64) -> std::result::Result<(), PlatformError> {
        if completion != self.next {
            return Err(PlatformError::Backend(
                io::Error::other("unexpected completion").into(),
            ));
        }
        // A real platform writes back the read value and advances the guest PC.
        self.next += 1;
        Ok(())
    }

    fn set_irq(&mut self, _: u32, _: bool) -> std::result::Result<(), PlatformError> {
        Ok(())
    }

    fn pause(&mut self) -> std::result::Result<(), PlatformError> {
        self.running = false;
        Ok(())
    }

    fn resume(&mut self) -> std::result::Result<(), PlatformError> {
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) {
        self.running = false;
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.stop();
        self.mappings.clear();
    }
}

struct Console;

impl Write for Console {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        io::stdout().write(b)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

impl SerialIo for Console {
    fn recv(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

struct NoNet;

impl NetDevice for NoNet {
    const MTU: u16 = 1500;

    fn mac_address(&self) -> [u8; 6] {
        [2, 0, 0, 0, 0, 1]
    }

    fn max_frame_len(&self) -> usize {
        1514
    }

    fn send(&mut self, _: &[u8]) -> std::result::Result<bool, NetError> {
        Ok(true)
    }

    fn recv(&mut self, _: &mut [u8]) -> std::result::Result<Option<usize>, NetError> {
        Ok(None)
    }
}

fn main() -> Result<()> {
    Vmm::new(
        VmConfig {
            memory_mib: 2,
            vcpu_count: 1,
        },
        BTreeMap::<String, Disk>::new(),
        None::<NoNet>,
        None,
        Console,
    )
    .run_with_platform(InProcess)
}
