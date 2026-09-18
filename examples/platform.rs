//! A complete in-process platform example. No hypervisor or guest image needed.
//! Run: cargo run -p w-vmm --example platform
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    time::Duration,
};
use vm_memory::{GuestMemoryMmap, GuestMemoryRegion, GuestRegionMmap};
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
    fn layout(&self, config: &VmConfig, needs: &DeviceRequirements) -> Result<MachineLayout> {
        ensure!(
            needs.devices.is_empty() && needs.hotplug.is_none(),
            "example supports only UART"
        );
        let size = config
            .memory_mib
            .checked_mul(1 << 20)
            .ok_or_else(|| anyhow::anyhow!("RAM overflow"))?;
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
    fn create(&self, config: &VmConfig) -> Result<Vm> {
        ensure!(config.vcpu_count == 1, "example supports one CPU");
        Ok(Vm {
            mappings: BTreeMap::new(),
            running: false,
            next: 0,
        })
    }
}
impl Mapper for Vm {
    fn map(&mut self, region: &GuestRegionMmap) -> Result<()> {
        ensure!(!self.running, "mapping requires quiescence");
        let address = region.start_addr().0;
        ensure!(!self.mappings.contains_key(&address), "already mapped");
        // A real hypervisor maps region.as_ptr() here, borrowing its allocation.
        self.mappings.insert(address, region.len());
        Ok(())
    }
    fn unmap(&mut self, region: &GuestRegionMmap) -> Result<()> {
        ensure!(!self.running, "unmapping requires quiescence");
        self.mappings.remove(&region.start_addr().0);
        Ok(())
    }
}
impl VirtualMachine for Vm {
    type Completion = usize;
    fn prepare(&mut self, _: &GuestMemoryMmap, _: &MachineLayout) -> Result<()> {
        // Real platforms load their image, initialize registers and interrupt
        // controllers, and create parked vCPUs here. No CPU may execute yet.
        Ok(())
    }
    fn start(&mut self) -> Result<()> {
        self.running = true;
        Ok(())
    }
    fn poll_event(&mut self, _: Duration) -> Result<Option<Event<usize>>> {
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
    fn complete_io(&mut self, completion: usize, _: u64) -> Result<()> {
        ensure!(completion == self.next, "unexpected completion");
        // A real platform writes back the read value and advances the guest PC.
        self.next += 1;
        Ok(())
    }
    fn set_irq(&mut self, _: u32, _: bool) -> Result<()> {
        Ok(())
    }
    fn pause(&mut self) -> Result<()> {
        self.running = false;
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
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
    fn send(&mut self, _: &[u8]) -> Result<bool> {
        Ok(true)
    }
    fn recv(&mut self, _: &mut [u8]) -> Result<Option<usize>> {
        Ok(None)
    }
}
fn main() -> Result<()> {
    Vmm::new(VmConfig {
        memory_mib: 2,
        vcpu_count: 1,
    })
    .run_with_platform(
        InProcess,
        BTreeMap::<String, Disk>::new(),
        None::<NoNet>,
        None,
        Console,
    )
}
