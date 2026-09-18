//! HVF workers retain vCPU thread ownership and PSCI handling.
mod cpus;
mod hvf;
use super::*;
use crate::boot;
use std::sync::mpsc;
use vm_memory::GuestRegionMmap;

/// Built-in Apple Silicon platform using the bundled ARM64 guest.
pub struct Hvf;

pub struct Vm {
    cpus: Option<cpus::Cpus>,
    rx: Option<mpsc::Receiver<cpus::Event>>,
    inner: hvf::Vm,
    config: VmConfig,
    boot: Option<boot::Layout>,
}

impl Platform for Hvf {
    type Vm = Vm;

    fn layout(&self, config: &VmConfig, devices: &DeviceRequirements) -> Result<MachineLayout> {
        let boot = boot::Layout::new(config.memory_mib, boot::KERNEL, boot::INITRD.len())?;
        ensure!(
            (1..=hvf::max_vcpus()?).contains(&config.vcpu_count),
            "vCPU count outside HVF supported range"
        );
        let ram = MemoryRange {
            address: boot::RAM,
            size: boot.size as u64,
        };
        let hotplug = devices
            .hotplug
            .as_ref()
            .map(|n| -> Result<_> {
                let address = ram
                    .address
                    .checked_add(ram.size)
                    .and_then(|a| a.checked_next_multiple_of(n.alignment))
                    .ok_or_else(|| anyhow::anyhow!("hotplug overflow"))?;
                Ok(MemoryRange {
                    address,
                    size: n.capacity,
                })
            })
            .transpose()?;
        for r in std::iter::once(&ram).chain(hotplug.iter()) {
            boot::validate_ipa_range(r.address, r.size, hvf::ipa_bits()?)?;
        }
        Ok(MachineLayout {
            ram: vec![ram],
            hotplug,
            uart: IoRegion {
                space: IoSpace::Mmio,
                address: boot::UART,
                size: 8,
                irq: boot::UART_IRQ,
            },
            virtio: boot::virtio_regions(devices.devices.len())?
                .iter()
                .map(|r| IoRegion {
                    space: IoSpace::Mmio,
                    address: r.address,
                    size: boot::VIRTIO_STRIDE,
                    irq: r.irq,
                })
                .collect(),
        })
    }

    fn create(&self, config: &VmConfig) -> Result<Vm> {
        Ok(Vm {
            cpus: None,
            rx: None,
            inner: hvf::Vm::new()?,
            config: config.clone(),
            boot: None,
        })
    }
}

impl Mapper for Vm {
    fn map(&mut self, region: &GuestRegionMmap) -> Result<()> {
        self.inner.map(region)
    }

    fn unmap(&mut self, region: &GuestRegionMmap) -> Result<()> {
        self.inner.unmap(region)
    }
}

impl VirtualMachine for Vm {
    type Completion = mpsc::SyncSender<u64>;

    fn prepare(&mut self, memory: &GuestMemoryMmap, layout: &MachineLayout) -> Result<()> {
        let boot = boot::Layout::new(self.config.memory_mib, boot::KERNEL, boot::INITRD.len())?;
        let regions = layout
            .virtio
            .iter()
            .map(|r| boot::VirtioRegion {
                address: r.address,
                irq: r.irq,
            })
            .collect::<Vec<_>>();
        let (redist, timers) = self.inner.gic()?;
        hvf::validate_irqs(&regions)?;
        let dtb = boot::fdt(
            &boot,
            &regions,
            redist,
            timers,
            self.config.vcpu_count,
            layout.hotplug.is_some(),
        )?;
        boot::load(memory, &boot, &dtb)?;
        let (tx, rx) = mpsc::channel();
        self.cpus = Some(cpus::Cpus::create(self.config.vcpu_count, tx)?);
        self.rx = Some(rx);
        self.boot = Some(boot);
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        let b = self
            .boot
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("VM not prepared"))?;
        self.cpus.as_ref().unwrap().start(b.entry, b.dtb);
        Ok(())
    }

    fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event<Self::Completion>>> {
        match self.rx.as_ref().unwrap().recv_timeout(timeout) {
            Ok(cpus::Event::Shutdown) => Ok(Some(Event::Shutdown)),
            Ok(cpus::Event::Failed(e)) => Err(e),
            Ok(cpus::Event::Access(a)) => Ok(Some(Event::Io(IoAccess {
                space: IoSpace::Mmio,
                address: a.addr,
                width: a.width,
                write: a.write,
                value: a.value,
                completion: a.reply,
            }))),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("all vCPU event senders disconnected")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if self.cpus.as_ref().unwrap().stopping() {
                    if let Ok(cpus::Event::Failed(e)) = self.rx.as_ref().unwrap().try_recv() {
                        return Err(e);
                    }
                    Ok(Some(Event::Shutdown))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn complete_io(&mut self, completion: Self::Completion, value: u64) -> Result<()> {
        let _ = completion.send(value);
        Ok(())
    }

    fn set_irq(&mut self, irq: u32, level: bool) -> Result<()> {
        hvf::spi(irq, level)
    }

    fn pause(&mut self) -> Result<()> {
        self.cpus.as_ref().unwrap().shared.pause()
    }

    fn resume(&mut self) -> Result<()> {
        self.cpus.as_ref().unwrap().shared.resume();
        Ok(())
    }

    fn stop(&mut self) {
        self.cpus.take();
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.stop();
    }
}
