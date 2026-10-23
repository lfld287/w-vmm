//! Architecture-independent device runtime.
use crate::Result;
use crate::error::{PlatformError, RuntimeError};
use crate::{
    VirtioMem, VmConfig,
    devices::{Device, block::Block, mmio::Mmio, net::Net},
    net::NetDevice,
    platform::*,
    serial::{SerialIo, poll_input},
    storage::BlockStorage,
};
use std::{collections::BTreeMap, ops::ControlFlow, sync::Arc, time::Duration};
use vm_memory::{GuestAddress, GuestMemoryMmap, GuestRegionMmap};
use vm_superio::{Serial, Trigger};

pub(crate) fn run<P: Platform, BS: BlockStorage, ND: NetDevice, SI: SerialIo>(
    config: &VmConfig,
    control: &crate::VmControl,
    platform: P,
    blocks: BTreeMap<String, BS>,
    net: Option<ND>,
    memory: Option<VirtioMem>,
    serial: SI,
) -> Result<()> {
    let mut runtime = Runtime::new(config, &platform, blocks, net, memory)?;
    let result = if control.stopping() {
        Ok(())
    } else {
        runtime.init(config, &platform, control).and_then(|view| {
            control.running();
            runtime.run_loop(view, serial, control)
        })
    };
    control.cleaning();
    let (fault, cleanup_result) = runtime.cleanup();
    control.cleanup_result(&cleanup_result);
    // Every exit path reaches here; the first error in this order is the true cause.
    result.and(fault.map_err(Into::into)).and(cleanup_result)
}

// Fields drop in declaration order. If a backend callback panics, unwinding must
// stop and destroy the VM before devices (which own dynamic RAM) and base RAM.
struct Runtime<VM: VirtualMachine, BS: BlockStorage, ND: NetDevice> {
    vm: Option<VM>,
    layout: MachineLayout,
    devices: Vec<Mmio<Device<BS, ND>>>,
    base: Vec<Arc<GuestRegionMmap>>,
    mapped: usize,
    has_memory: bool,
    strict: bool,
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
                alignment: m.0.alignment(),
            }),
        };
        let layout = platform.layout(config, &needs)?;
        layout.validate(config, &needs)?;
        if let Some(m) = &mut memory {
            m.0.attach_at(layout.hotplug.unwrap().address)?;
        }
        let has_memory = memory.is_some();
        let devices: Vec<Mmio<Device<BS, ND>>> = blocks
            .into_iter()
            .map(|(n, b)| Block::new(n, b).map(Device::Block))
            .chain(net.map(|n| Net::new(n).map(Device::Net)))
            .chain(memory.map(|m| Ok(Device::Mem(m.0))))
            .map(|d| d.map(|d| Mmio::new(d).with_strict(config.strict)))
            .collect::<std::result::Result<_, crate::error::DeviceError>>()?;
        Ok(Self {
            vm: None,
            layout,
            devices,
            base: Vec::new(),
            mapped: 0,
            has_memory,
            strict: config.strict,
        })
    }

    fn init<P: Platform<Vm = VM>>(
        &mut self,
        config: &VmConfig,
        platform: &P,
        control: &crate::VmControl,
    ) -> Result<GuestMemoryMmap> {
        for r in &self.layout.ram {
            self.base.push(Arc::new(
                GuestRegionMmap::from_range(GuestAddress(r.address), r.size as usize, None)
                    .map_err(crate::error::MemoryError::from)?,
            ));
        }
        let view = GuestMemoryMmap::from_arc_regions(self.base.clone())
            .map_err(crate::error::MemoryError::from)?;
        if control.stopping() {
            return Ok(view);
        }
        self.vm = Some(platform.create(config)?);
        let vm = self.vm.as_mut().unwrap();
        for region in &self.base {
            if control.stopping() {
                return Ok(view);
            }
            vm.map(region.clone())?;
            self.mapped += 1;
        }
        if control.stopping() {
            return Ok(view);
        }
        vm.prepare(&view, &self.layout)?;
        if control.stopping() {
            return Ok(view);
        }
        vm.start()?;
        for d in &self.devices {
            if let Device::Mem(m) = &d.device {
                m.start();
            }
        }
        Ok(view)
    }

    fn run_loop<SI: SerialIo>(
        &mut self,
        mut view: GuestMemoryMmap,
        serial: SI,
        control: &crate::VmControl,
    ) -> Result<()> {
        let mut serial = Serial::new(Irq, serial);
        let mut paused = false;
        loop {
            if control.stopping() {
                break;
            }
            if let Some(request) = control.next() {
                let vm = self.vm.as_mut().unwrap();
                // Ok(true): the guest or a vCPU is already ending the VM.
                let stopping = match request.operation {
                    crate::control::Operation::Pause if !paused => {
                        vm.pause().map(|o| o == PauseOutcome::Stopping)
                    }
                    crate::control::Operation::Resume if paused => vm.resume().map(|()| false),
                    _ => Ok(false),
                }
                .map_err(crate::Error::from);
                if let Ok(true) = stopping {
                    // Terminate without resuming; stop reports any fault.
                    control.reject_stopping(request);
                    break;
                }
                let outcome = stopping.map(drop);
                if outcome.is_ok() {
                    paused = matches!(request.operation, crate::control::Operation::Pause);
                }
                control.complete(request, &outcome);
                outcome?;
                continue;
            }
            if paused {
                control.wait();
                continue;
            }
            poll_input(&mut serial)?;
            self.vm
                .as_mut()
                .unwrap()
                .set_irq(self.layout.uart.irq, uart_level(&serial))?;
            for device in &mut self.devices {
                device.poll(&view)?;
            }
            if self.process_memory(&mut view)?.is_break() {
                break;
            }
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

    /// Break means the VM began stopping before the transaction; it never resumes.
    fn process_memory(&mut self, view: &mut GuestMemoryMmap) -> Result<ControlFlow<()>> {
        if !self.has_memory {
            return Ok(ControlFlow::Continue(()));
        }
        let vm = self.vm.as_mut().unwrap();
        let last = self.devices.len() - 1;
        let active = self.devices[last].active();
        if let Device::Mem(d) = &mut self.devices[last].device
            && d.sync_target(active)
        {
            self.devices[last].config_changed();
        }
        if !active {
            return Ok(ControlFlow::Continue(()));
        }
        let available = self.devices[last].queues.available(0, view);
        if !matches!(self.devices[last].contain(available)?, Some(n) if n != 0) {
            return Ok(ControlFlow::Continue(()));
        }
        if vm.pause()? == PauseOutcome::Stopping {
            return Ok(ControlFlow::Break(()));
        }
        // Scan each device separately: in lenient mode a corrupt queue isolates
        // only its device, whose stopped queues then need no pins.
        let mut pinned = Vec::new();
        for device in self.devices.iter_mut().filter(|d| !d.needs_reset()) {
            let pins = device.queues.pinned(view);
            pinned.extend(device.contain(pins)?.into_iter().flatten());
        }
        let device = &mut self.devices[last];
        if !device.needs_reset() {
            let Device::Mem(d) = &mut device.device else {
                unreachable!()
            };
            let result = d.process(&mut device.queues, vm, view, &pinned);
            device.contain(result)?;
        }
        vm.resume()?;
        Ok(ControlFlow::Continue(()))
    }

    fn handle_io<SI: SerialIo>(
        &mut self,
        a: IoAccess<VM::Completion>,
        view: &GuestMemoryMmap,
        serial: &mut Serial<Irq, vm_superio::serial::NoEvents, SI>,
    ) -> Result<()> {
        let within = |r: &IoRegion| {
            a.space == r.space && a.address.checked_sub(r.address).is_some_and(|o| o < r.size)
        };
        let fits = |r: &IoRegion| {
            matches!(a.width, 1 | 2 | 4 | 8)
                && (a.address - r.address)
                    .checked_add(a.width as u64)
                    .is_some_and(|e| e <= r.size)
        };
        // None: the access starts in a device window but decodes to no register.
        let (value, irq, level) = if within(&self.layout.uart) {
            let offset = a.address - self.layout.uart.address;
            let value = if offset < 8 && a.width == 1 {
                Some(if a.write {
                    serial
                        .write(offset as u8, a.value as u8)
                        .map_err(crate::error::SerialError::from)?;
                    0
                } else {
                    serial.read(offset as u8) as u64
                })
            } else {
                None
            };
            (value, self.layout.uart.irq, uart_level(serial))
        } else if let Some(slot) = self.layout.virtio.iter().position(within) {
            let region = self.layout.virtio[slot];
            let device = &mut self.devices[slot];
            let offset = a.address - region.address;
            let value = if !fits(&region) {
                None
            } else if !a.write {
                device.read(offset, a.width)
            } else if a.width == 4 {
                device.write(offset, a.value as u32, view)?;
                Some(0)
            } else {
                None
            };
            (value, region.irq, device.interrupt_pending())
        } else {
            return Err(RuntimeError::UnmappedIo {
                address: a.address,
                width: a.width,
            }
            .into());
        };
        let value = match value {
            Some(value) => value,
            None if self.strict => {
                return Err(RuntimeError::InvalidAccess {
                    address: a.address,
                    width: a.width,
                    write: a.write,
                }
                .into());
            }
            // Lenient: no side effects; reads see an unclaimed bus.
            None if a.write => 0,
            None if a.width >= 8 => u64::MAX,
            None => (1 << (8 * a.width)) - 1,
        };
        let vm = self.vm.as_mut().unwrap();
        // An InterruptACK, IIR read or notification may change the level; the
        // vCPU must not resume and observe the device before the GIC does.
        vm.set_irq(irq, level)?;
        vm.complete_io(a.completion, value)?;
        Ok(())
    }

    /// Returns the vCPU fault from `stop` and the first cleanup error. Every step
    /// runs regardless of earlier failures.
    fn cleanup(mut self) -> (std::result::Result<(), PlatformError>, Result<()>) {
        let fault = self.vm.as_mut().map_or(Ok(()), |vm| vm.stop());
        let mut cleanup = Ok(());
        for device in &self.devices {
            if let Err(e) = device.flush()
                && cleanup.is_ok()
            {
                cleanup = Err(e.into());
            }
        }
        if let Some(vm) = &mut self.vm {
            for d in &self.devices {
                if let Device::Mem(m) = &d.device
                    && let Err(e) = m.stop(vm)
                    && cleanup.is_ok()
                {
                    cleanup = Err(e.into());
                }
            }
            for r in self.base.iter().take(self.mapped) {
                if let Err(e) = vm.unmap(r)
                    && cleanup.is_ok()
                {
                    cleanup = Err(e.into());
                }
            }
        }
        drop(self.vm);
        drop(self.base);
        drop(self.devices);
        (fault, cleanup)
    }
}

fn uart_level<SI: SerialIo>(serial: &Serial<Irq, vm_superio::serial::NoEvents, SI>) -> bool {
    serial.state().interrupt_identification & 1 == 0
}

struct Irq;

impl Trigger for Irq {
    type E = std::io::Error;

    fn trigger(&self) -> std::io::Result<()> {
        Ok(())
    }
}
