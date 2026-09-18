//! Architecture-independent device runtime.
use crate::{
    VirtioMem, VmConfig,
    devices::{Device, block::Block, mmio::Mmio, net::Net},
    net::NetDevice,
    platform::*,
    serial::{SerialIo, poll_input},
    storage::BlockStorage,
};
use anyhow::{Result, bail, ensure};
use std::{collections::BTreeMap, time::Duration};
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};
use vm_superio::{Serial, Trigger};

pub(crate) fn run<P: Platform, BS: BlockStorage, ND: NetDevice, SI: SerialIo>(
    config: &VmConfig,
    platform: P,
    blocks: BTreeMap<String, BS>,
    net: Option<ND>,
    memory: Option<VirtioMem>,
    serial: SI,
) -> Result<()> {
    let mut runtime = Runtime::new(config, &platform, blocks, net, memory)?;
    let result = runtime
        .init(config, &platform)
        .and_then(|view| runtime.run_loop(view, serial));
    let cleanup_result = runtime.cleanup();
    result.and(cleanup_result)
}

struct Runtime<VM: VirtualMachine, BS: BlockStorage, ND: NetDevice> {
    layout: MachineLayout,
    devices: Vec<Mmio<Device<BS, ND>>>,
    vm: Option<VM>,
    base: Option<GuestMemoryMmap>,
    mapped: usize,
    has_memory: bool,
}

impl<VM: VirtualMachine, BS: BlockStorage, ND: NetDevice> Runtime<VM, BS, ND> {
    fn new<P: Platform<Vm = VM>>(
        config: &VmConfig,
        platform: &P,
        blocks: BTreeMap<String, BS>,
        net: Option<ND>,
        mut memory: Option<VirtioMem>,
    ) -> Result<Self> {
        let mut kinds: Vec<_> = blocks.keys().cloned().map(DeviceKind::Block).collect();
        if net.is_some() {
            kinds.push(DeviceKind::Net);
        }
        if memory.is_some() {
            kinds.push(DeviceKind::Memory);
        }
        let needs = DeviceRequirements {
            devices: kinds,
            hotplug: memory.as_ref().map(|m| MemoryRequirement {
                capacity: m.control().status().region_size_mib * crate::memory::MIB,
                alignment: crate::memory::HOTPLUG_BLOCK_SIZE,
            }),
        };
        let layout = platform.layout(config, &needs)?;
        layout.validate(config, &needs)?;
        if let Some(m) = &mut memory {
            m.0.attach_at(layout.hotplug.unwrap().address)?;
        }
        let has_memory = memory.is_some();
        let mut devices: Vec<Mmio<Device<BS, ND>>> = blocks
            .into_iter()
            .map(|(n, b)| Block::new(n, b).map(|d| Mmio::new(Device::Block(d))))
            .collect::<Result<_>>()?;
        if let Some(n) = net {
            devices.push(Mmio::new(Device::Net(Net::new(n)?)));
        }
        if let Some(m) = memory {
            devices.push(Mmio::new(Device::Mem(m.0)));
        }
        Ok(Self {
            layout,
            devices,
            vm: None,
            base: None,
            mapped: 0,
            has_memory,
        })
    }

    fn init<P: Platform<Vm = VM>>(
        &mut self,
        config: &VmConfig,
        platform: &P,
    ) -> Result<GuestMemoryMmap> {
        let ranges = self
            .layout
            .ram
            .iter()
            .map(|r| (GuestAddress(r.address), r.size as usize))
            .collect::<Vec<_>>();
        self.base = Some(GuestMemoryMmap::from_ranges(&ranges)?);
        let view = self.base.as_ref().unwrap().clone();
        self.vm = Some(platform.create(config)?);
        let vm = self.vm.as_mut().unwrap();
        for region in self.base.as_ref().unwrap().iter() {
            vm.map(region)?;
            self.mapped += 1;
        }
        vm.prepare(&view, &self.layout)?;
        vm.start()?;
        for d in &self.devices {
            if let Device::Mem(m) = &d.device {
                m.start();
            }
        }
        Ok(view)
    }

    fn run_loop<SI: SerialIo>(&mut self, mut view: GuestMemoryMmap, serial: SI) -> Result<()> {
        let mut serial = Serial::new(Irq, serial);
        loop {
            if poll_input(&mut serial)? {
                break;
            }
            self.vm.as_mut().unwrap().set_irq(
                self.layout.uart.irq,
                serial.state().interrupt_identification & 1 == 0,
            )?;
            for device in &mut self.devices {
                device.poll(&view)?;
            }
            self.process_memory(&mut view)?;
            let vm = self.vm.as_mut().unwrap();
            for (device, region) in self.devices.iter().zip(&self.layout.virtio) {
                vm.set_irq(region.irq, device.interrupt_pending())?;
            }
            match vm.poll_event(Duration::from_millis(2))? {
                Some(Event::Shutdown) => break,
                None => (),
                Some(Event::Io(a)) => self.handle_io(a, &view, &mut serial)?,
            }
        }
        Ok(())
    }

    fn process_memory(&mut self, view: &mut GuestMemoryMmap) -> Result<()> {
        if !self.has_memory {
            return Ok(());
        }
        let vm = self.vm.as_mut().unwrap();
        let last = self.devices.len() - 1;
        let active = self.devices[last].active();
        if let Device::Mem(d) = &mut self.devices[last].device
            && d.sync_target(active)
        {
            self.devices[last].config_changed();
        }
        if active && self.devices[last].queues.available(0, view)? != 0 {
            vm.pause()?;
            let pinned = self
                .devices
                .iter()
                .map(|d| d.queues.pinned(view))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            let device = &mut self.devices[last];
            if let Device::Mem(d) = &mut device.device {
                d.process(&mut device.queues, vm, view, &pinned)?;
            }
            vm.resume()?;
        }
        Ok(())
    }

    fn handle_io<SI: SerialIo>(
        &mut self,
        a: IoAccess<VM::Completion>,
        view: &GuestMemoryMmap,
        serial: &mut Serial<Irq, vm_superio::serial::NoEvents, SI>,
    ) -> Result<()> {
        ensure!(matches!(a.width, 1 | 2 | 4 | 8), "invalid I/O width");
        let contains = |r: &IoRegion| {
            a.space == r.space
                && a.address >= r.address
                && a.address
                    .checked_add(a.width as u64)
                    .is_some_and(|e| e <= r.address + r.size)
        };
        let value = if contains(&self.layout.uart)
            && a.address - self.layout.uart.address < 8
            && a.width == 1
        {
            let offset = (a.address - self.layout.uart.address) as u8;
            if a.write {
                serial.write(offset, a.value as u8)?;
                0
            } else {
                serial.read(offset) as u64
            }
        } else if let Some(slot) = self.layout.virtio.iter().position(contains) {
            let offset = a.address - self.layout.virtio[slot].address;
            if a.write {
                ensure!(a.width == 4, "virtio MMIO writes must be 32 bit");
                self.devices[slot].write(offset, a.value as u32, view)?;
                0
            } else {
                self.devices[slot].read(offset, a.width)
            }
        } else {
            bail!("unmapped I/O {:#x}, size {}", a.address, a.width);
        };
        self.vm.as_mut().unwrap().complete_io(a.completion, value)?;
        Ok(())
    }

    fn cleanup(mut self) -> Result<()> {
        // Keep both base and dynamic allocations alive through VM destruction.
        if let Some(vm) = &mut self.vm {
            vm.stop();
        }
        let mut cleanup = Ok(());
        for device in &self.devices {
            if let Err(e) = device.flush()
                && cleanup.is_ok()
            {
                cleanup = Err(e);
            }
        }
        if let Some(vm) = &mut self.vm {
            for d in &self.devices {
                if let Device::Mem(m) = &d.device {
                    m.stop(vm);
                }
            }
            if let Some(base) = &self.base {
                for r in base.iter().take(self.mapped) {
                    if let Err(e) = vm.unmap(r)
                        && cleanup.is_ok()
                    {
                        cleanup = Err(e);
                    }
                }
            }
        }
        drop(self.vm);
        drop(self.base);
        drop(self.devices);
        cleanup
    }
}

struct Irq;

impl Trigger for Irq {
    type E = std::io::Error;

    fn trigger(&self) -> std::io::Result<()> {
        Ok(())
    }
}
