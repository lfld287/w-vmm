//! HVF workers retain vCPU thread ownership and PSCI handling.
mod cpus;
mod hvf;
use super::*;
use crate::boot;
use std::sync::{Arc, mpsc};
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
        if !(1..=hvf::max_vcpus()?).contains(&config.vcpu_count) {
            return Err(PlatformError::UnsupportedVcpuCount);
        }
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
                    .ok_or_else(|| PlatformError::HotplugOverflow)?;
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
                size: boot::UART_SIZE,
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
    fn map(
        &mut self,
        region: Arc<GuestRegionMmap>,
    ) -> std::result::Result<(), crate::error::MemoryError> {
        self.inner.map(region)
    }

    fn unmap(
        &mut self,
        region: &GuestRegionMmap,
    ) -> std::result::Result<(), crate::error::MemoryError> {
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
            .ok_or_else(|| PlatformError::VmNotPrepared)?;
        self.cpus.as_ref().unwrap().start(b.entry, b.dtb);
        Ok(())
    }

    fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event<Self::Completion>>> {
        let cpus = self.cpus.as_ref().unwrap();
        poll(self.rx.as_ref().unwrap(), timeout, || cpus.stopping())
    }

    fn complete_io(&mut self, completion: Self::Completion, value: u64) -> Result<()> {
        let _ = completion.send(value);
        Ok(())
    }

    fn set_irq(&mut self, irq: u32, level: bool) -> Result<()> {
        hvf::spi(irq, level)
    }

    fn pause(&mut self) -> Result<PauseOutcome> {
        Ok(self.cpus.as_ref().unwrap().shared.pause())
    }

    fn resume(&mut self) -> Result<()> {
        self.cpus.as_ref().unwrap().shared.resume();
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        // Join every vCPU first, so no event can be sent after the drain.
        let joined = self.cpus.take().map_or(Ok(()), |mut cpus| cpus.join());
        self.rx.as_ref().map_or(Ok(()), drain).and(joined)
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Faults are returned as soon as they are received. Once vCPUs are stopping,
/// an idle or closed channel means shutdown; `stop` collects any remaining fault.
fn poll(
    rx: &mpsc::Receiver<cpus::Event>,
    timeout: Duration,
    stopping: impl Fn() -> bool,
) -> Result<Option<Event<mpsc::SyncSender<u64>>>> {
    match rx.recv_timeout(timeout) {
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
        Err(_) if stopping() => Ok(Some(Event::Shutdown)),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(PlatformError::EventChannelDisconnected),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
    }
}

/// Consume every queued event after vCPUs are joined and return the first fault
/// that `poll` has not reported. Pending accesses and shutdowns are discarded.
fn drain(rx: &mpsc::Receiver<cpus::Event>) -> Result<()> {
    let mut fault = Ok(());
    while let Ok(event) = rx.try_recv() {
        if let cpus::Event::Failed(e) = event
            && fault.is_ok()
        {
            fault = Err(e);
        }
    }
    fault
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(addr: u64) -> (cpus::Event, mpsc::Receiver<u64>) {
        let (reply, rx) = mpsc::sync_channel(1);
        let event = cpus::Event::Access(cpus::Access {
            addr,
            width: 4,
            write: false,
            value: 0,
            reply,
        });
        (event, rx)
    }

    fn fault(address: u64) -> cpus::Event {
        cpus::Event::Failed(PlatformError::MmioReplyDisconnected { address })
    }

    fn reported(result: Result<()>) -> Option<u64> {
        match result {
            Ok(()) => None,
            Err(PlatformError::MmioReplyDisconnected { address }) => Some(address),
            Err(e) => panic!("unexpected {e}"),
        }
    }

    #[test]
    fn drain_returns_first_fault_behind_access_and_shutdown_once() {
        let (tx, rx) = mpsc::channel();
        let (event, reply) = access(0x10);
        tx.send(event).unwrap();
        tx.send(cpus::Event::Shutdown).unwrap();
        tx.send(fault(1)).unwrap();
        tx.send(fault(2)).unwrap();
        drop(tx);
        assert_eq!(reported(drain(&rx)), Some(1));
        // The discarded access releases its waiter; repeated stops succeed.
        assert!(reply.recv().is_err());
        assert_eq!(reported(drain(&rx)), None);
        assert_eq!(reported(drain(&rx)), None);
    }

    #[test]
    fn polled_fault_is_not_reported_again() {
        let (tx, rx) = mpsc::channel();
        tx.send(fault(1)).unwrap();
        tx.send(cpus::Event::Shutdown).unwrap();
        tx.send(fault(2)).unwrap();
        let running = || false;
        assert!(matches!(
            poll(&rx, Duration::ZERO, running),
            Err(PlatformError::MmioReplyDisconnected { address: 1 })
        ));
        assert!(matches!(
            poll(&rx, Duration::ZERO, running),
            Ok(Some(Event::Shutdown))
        ));
        assert_eq!(reported(drain(&rx)), Some(2));
        assert_eq!(reported(drain(&rx)), None);
    }

    #[test]
    fn poll_orders_events_and_treats_idle_stopping_as_shutdown() {
        let (tx, rx) = mpsc::channel();
        assert!(matches!(poll(&rx, Duration::ZERO, || false), Ok(None)));
        let (event, _reply) = access(0x20);
        tx.send(event).unwrap();
        tx.send(fault(3)).unwrap();
        // Queued events are delivered before the stopping state is considered.
        assert!(matches!(
            poll(&rx, Duration::ZERO, || true),
            Ok(Some(Event::Io(IoAccess { address: 0x20, .. })))
        ));
        assert!(poll(&rx, Duration::ZERO, || true).is_err());
        assert!(matches!(
            poll(&rx, Duration::ZERO, || true),
            Ok(Some(Event::Shutdown))
        ));
        drop(tx);
        assert!(matches!(
            poll(&rx, Duration::ZERO, || true),
            Ok(Some(Event::Shutdown))
        ));
        assert!(matches!(
            poll(&rx, Duration::ZERO, || false),
            Err(PlatformError::EventChannelDisconnected)
        ));
    }
}
