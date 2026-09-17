//! HVF workers own vCPUs; the caller exclusively owns all device backends.
mod cpus;
mod hvf;
use super::VmRuntime;
use crate::{
    MemoryControl, MemoryLifecycle, VmConfig, boot,
    devices::{Device, block::Block, memory::Mem, mmio::Mmio, net::Net},
    net::NetDevice,
    serial::{SerialIo, poll_input},
    storage::BlockStorage,
};
use anyhow::{Result, bail, ensure};
use std::{collections::BTreeMap, sync::mpsc, time::Duration};
use vm_memory::{GuestAddress, GuestMemoryMmap};
use vm_superio::{Serial, Trigger};
pub(super) struct Backend;
impl VmRuntime for Backend {
    fn run<BS: BlockStorage, ND: NetDevice, SI: SerialIo>(
        config: &VmConfig,
        control: Option<MemoryControl>,
        blocks: BTreeMap<String, BS>,
        net: Option<ND>,
        serial: SI,
    ) -> Result<()> {
        let layout = boot::Layout::new(config.memory_mib, boot::KERNEL, boot::INITRD.len())?;
        ensure!(
            (1..=hvf::max_vcpus()?).contains(&config.vcpu_count),
            "vCPU count outside HVF supported range"
        );
        if let Some(c) = &config.virtio_mem {
            c.validate(config.memory_mib)?;
        }
        let regions = boot::virtio_regions(
            blocks.len() + usize::from(net.is_some()) + usize::from(control.is_some()),
        )?;
        let mut devices: Vec<Mmio<Device<BS, ND>>> = blocks
            .into_iter()
            .map(|(name, disk)| Block::new(name, disk).map(|b| Mmio::new(Device::Block(b))))
            .collect::<Result<_>>()?;
        if let Some(net) = net {
            devices.push(Mmio::new(Device::Net(Net::new(net)?)));
        }
        if let Some(c) = &control {
            devices.push(Mmio::new(Device::Mem(Mem::new(
                config.memory_mib,
                c.clone(),
            ))));
        }
        // All failures from VM setup onward still attempt every backend flush.
        let mut vm_slot = None;
        let result = (|| -> Result<()> {
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(boot::RAM), layout.size)])?;
            vm_slot = Some(hvf::Vm::new(mem)?);
            let vm = vm_slot.as_mut().unwrap();
            let (redist, timers) = vm.gic()?;
            hvf::validate_irqs(&regions)?;
            let dtb = boot::fdt(
                &layout,
                &regions,
                redist,
                timers,
                config.vcpu_count,
                control.is_some(),
            )?;
            boot::load(vm.memory(), &layout, &dtb)?;
            let (tx, rx) = mpsc::channel();
            let cpus = cpus::Cpus::create(config.vcpu_count, tx)?;
            if let Some(c) = &control {
                c.lock().lifecycle = MemoryLifecycle::Running;
            }
            cpus.start(layout.entry, layout.dtb);
            let mut serial = Serial::new(Irq, serial);
            loop {
                if poll_input(&mut serial)? {
                    break;
                }
                hvf::spi(
                    boot::UART_IRQ,
                    serial.state().interrupt_identification & 1 == 0,
                )?;
                for device in &mut devices {
                    device.poll(vm.memory())?;
                }
                if control.is_some() {
                    let last = devices.len() - 1;
                    let active = devices[last].active();
                    if let Device::Mem(d) = &mut devices[last].device
                        && d.sync_target(active)
                    {
                        devices[last].config_changed();
                    }
                    if active && devices[last].queues.available(0, vm.memory())? != 0 {
                        let _pause = cpus.shared.pause()?;
                        let pinned = devices
                            .iter()
                            .map(|d| d.queues.pinned(vm.memory()))
                            .collect::<Result<Vec<_>>>()?
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>();
                        let device = &mut devices[last];
                        if let Device::Mem(d) = &mut device.device {
                            d.process(&mut device.queues, &mut vm.mem, &pinned)?;
                        }
                    }
                }
                for (device, region) in devices.iter().zip(&regions) {
                    hvf::spi(region.irq, device.interrupt_pending())?;
                }
                match rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(cpus::Event::Failed(error)) => return Err(error),
                    Ok(cpus::Event::Shutdown) => break,
                    Ok(cpus::Event::Access(a)) => {
                        let value =
                            if (boot::UART..boot::UART + 8).contains(&a.addr) && a.width == 1 {
                                let offset = (a.addr - boot::UART) as u8;
                                if a.write {
                                    serial.write(offset, a.value as u8)?;
                                    0
                                } else {
                                    serial.read(offset) as u64
                                }
                            } else if let Some(slot) = a
                                .addr
                                .checked_sub(boot::VIRTIO_BASE)
                                .and_then(|o| usize::try_from(o / boot::VIRTIO_STRIDE).ok())
                                .filter(|&s| s < devices.len())
                            {
                                let offset = a.addr - regions[slot].address;
                                if a.write {
                                    ensure!(a.width == 4, "virtio MMIO writes must be 32 bit");
                                    devices[slot].write(offset, a.value as u32, vm.memory())?;
                                    0
                                } else {
                                    devices[slot].read(offset, a.width)
                                }
                            } else {
                                bail!("unmapped MMIO {:#x}, size {}", a.addr, a.width);
                            };
                        let _ = a.reply.send(value);
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        bail!("all vCPU event senders disconnected")
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if cpus.stopping() {
                            // Worker posts the failure/shutdown event before finishing.
                            if let Ok(cpus::Event::Failed(e)) = rx.try_recv() {
                                return Err(e);
                            }
                            break;
                        }
                    }
                }
            }
            Ok(())
        })();
        let mut flushed = Ok(());
        for device in &devices {
            if let Err(e) = device.flush() {
                eprintln!("final device flush: {e:#}");
                if flushed.is_ok() {
                    flushed = Err(e);
                }
            }
        }
        drop(vm_slot);
        result.and(flushed)
    }
}
struct Irq;
impl Trigger for Irq {
    type E = std::io::Error;
    fn trigger(&self) -> std::io::Result<()> {
        Ok(())
    }
}
