//! Architecture-independent device runtime.
use crate::{
    VirtioMem, VmConfig,
    devices::{Device, block::Block, mmio::Mmio, net::Net},
    memory::Mapper,
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
    mut memory: Option<VirtioMem>,
    serial: SI,
) -> Result<()> {
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
    // Keep both base and dynamic allocations alive through VM destruction.
    let mut base = None;
    let mut vm_slot = None;
    let mut mapped = 0;
    let result = (|| -> Result<()> {
        let ranges = layout
            .ram
            .iter()
            .map(|r| (GuestAddress(r.address), r.size as usize))
            .collect::<Vec<_>>();
        base = Some(GuestMemoryMmap::from_ranges(&ranges)?);
        let mut view = base.as_ref().unwrap().clone();
        vm_slot = Some(platform.create(config)?);
        let vm = vm_slot.as_mut().unwrap();
        for region in base.as_ref().unwrap().iter() {
            vm.map(region)?;
            mapped += 1;
        }
        vm.prepare(&view, &layout)?;
        vm.start()?;
        for d in &devices {
            if let Device::Mem(m) = &d.device {
                m.start();
            }
        }
        let mut serial = Serial::new(Irq, serial);
        loop {
            if poll_input(&mut serial)? {
                break;
            }
            vm.set_irq(
                layout.uart.irq,
                serial.state().interrupt_identification & 1 == 0,
            )?;
            for device in &mut devices {
                device.poll(&view)?;
            }
            if has_memory {
                let last = devices.len() - 1;
                let active = devices[last].active();
                if let Device::Mem(d) = &mut devices[last].device
                    && d.sync_target(active)
                {
                    devices[last].config_changed();
                }
                if active && devices[last].queues.available(0, &view)? != 0 {
                    vm.pause()?;
                    let pinned = devices
                        .iter()
                        .map(|d| d.queues.pinned(&view))
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>();
                    let device = &mut devices[last];
                    if let Device::Mem(d) = &mut device.device {
                        d.process(&mut device.queues, vm, &mut view, &pinned)?;
                    }
                    vm.resume()?;
                }
            }
            for (device, region) in devices.iter().zip(&layout.virtio) {
                vm.set_irq(region.irq, device.interrupt_pending())?;
            }
            match vm.poll_event(Duration::from_millis(2))? {
                Some(Event::Shutdown) => break,
                None => (),
                Some(Event::Io(a)) => {
                    ensure!(matches!(a.width, 1 | 2 | 4 | 8), "invalid I/O width");
                    let contains = |r: &IoRegion| {
                        a.space == r.space
                            && a.address >= r.address
                            && a.address
                                .checked_add(a.width as u64)
                                .is_some_and(|e| e <= r.address + r.size)
                    };
                    let value = if contains(&layout.uart)
                        && a.address - layout.uart.address < 8
                        && a.width == 1
                    {
                        let offset = (a.address - layout.uart.address) as u8;
                        if a.write {
                            serial.write(offset, a.value as u8)?;
                            0
                        } else {
                            serial.read(offset) as u64
                        }
                    } else if let Some(slot) = layout.virtio.iter().position(contains) {
                        let offset = a.address - layout.virtio[slot].address;
                        if a.write {
                            ensure!(a.width == 4, "virtio MMIO writes must be 32 bit");
                            devices[slot].write(offset, a.value as u32, &view)?;
                            0
                        } else {
                            devices[slot].read(offset, a.width)
                        }
                    } else {
                        bail!("unmapped I/O {:#x}, size {}", a.address, a.width);
                    };
                    vm.complete_io(a.completion, value)?;
                }
            }
        }
        Ok(())
    })();
    if let Some(vm) = &mut vm_slot {
        vm.stop();
    }
    let mut cleanup = Ok(());
    for device in &devices {
        if let Err(e) = device.flush() {
            if cleanup.is_ok() {
                cleanup = Err(e);
            }
        }
    }
    if let Some(vm) = &mut vm_slot {
        for d in &devices {
            if let Device::Mem(m) = &d.device {
                m.stop(vm);
            }
        }
        if let Some(base) = &base {
            for r in base.iter().take(mapped) {
                if let Err(e) = vm.unmap(r) {
                    if cleanup.is_ok() {
                        cleanup = Err(e);
                    }
                }
            }
        }
    }
    drop(vm_slot);
    drop(base);
    drop(devices);
    result.and(cleanup)
}
struct Irq;
impl Trigger for Irq {
    type E = std::io::Error;
    fn trigger(&self) -> std::io::Result<()> {
        Ok(())
    }
}
