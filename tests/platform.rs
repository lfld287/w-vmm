//! External platform exercising the real public runtime, without private devices.
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    io,
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use vm_memory::VolatileSlice;
use vm_memory::{
    Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion, GuestRegionMmap,
};
use w_vmm::error::{MemoryError, NetError, PlatformError, StorageError};
use w_vmm::{
    MemoryLifecycle, VirtioMem, VmConfig, Vmm, memory::Mapper, net::NetDevice, platform::*,
    serial::SerialIo, storage::BlockStorage,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Fault {
    None,
    Overlap,
    Overflow,
    Capacity,
    DeviceCount,
    Hotplug,
    Create,
    Prepare,
    Start,
    Pause,
    BaseMap,
    Map,
    Rollback,
    Poll,
    Resume,
    Flush,
    Unmap,
    Panic,
    /// The memory-transaction pause loses a race with guest shutdown.
    PauseStopping,
    /// The memory-transaction pause loses a race with a vCPU fault.
    PauseVcpuError,
    /// A vCPU fault surfaces only from stop after guest shutdown; flush fails too.
    StopVcpuError,
    /// Poll, stop, flush and unmap all fail; the poll error wins.
    Precedence,
    /// Invalid accesses, rejected register writes and corrupt queues; fatal
    /// only in strict mode.
    GuestErrors,
    /// The virtio-mem request has a short response buffer.
    MemoryRequest,
}

impl Fault {
    fn flush_fails(self) -> bool {
        matches!(
            self,
            Fault::Flush | Fault::StopVcpuError | Fault::Precedence
        )
    }

    fn vcpu_fault(self) -> bool {
        matches!(
            self,
            Fault::PauseVcpuError | Fault::StopVcpuError | Fault::Precedence
        )
    }
}

#[derive(Default)]
struct State {
    allocations: Vec<(u64, std::sync::Weak<vm_memory::MmapRegion>)>,
    /// Allocations already released when the VM was destroyed.
    released: Vec<u64>,
    log: Vec<String>,
    calls: usize,
    replies: Vec<u64>,
    output: Vec<u8>,
    irqs: Vec<(u32, bool)>,
    /// set_irq and complete_io calls in order.
    trace: Vec<String>,
    /// Expected reply values by completion.
    expect_reply: Vec<(usize, u64)>,
    /// The interrupt level set immediately before a completion.
    expect_irq: Vec<(usize, u32, bool)>,
}

type Shared = Rc<RefCell<State>>; // Deliberately !Send/!Sync.

struct TestPlatform {
    state: Shared,
    fault: Fault,
    port: bool,
    base: u64,
}

impl Platform for TestPlatform {
    type Vm = TestVm;

    fn layout(
        &self,
        _: &VmConfig,
        needs: &DeviceRequirements,
    ) -> Result<MachineLayout, PlatformError> {
        assert!(
            matches!(&needs.devices[..], [DeviceKind::Block(a), DeviceKind::Block(b), DeviceKind::Memory] if a == "a" && b == "z")
        );
        let h = needs.hotplug.as_ref().unwrap();
        assert_eq!((h.capacity, h.alignment), (128 << 20, 128 << 20));
        let mut l = MachineLayout {
            ram: vec![
                MemoryRange {
                    address: self.base,
                    size: 1 << 20,
                },
                MemoryRange {
                    address: self.base + (2 << 20),
                    size: 1 << 20,
                },
            ],
            uart: IoRegion {
                space: if self.port {
                    IoSpace::Port
                } else {
                    IoSpace::Mmio
                },
                address: 0x3f8,
                size: 8,
                irq: 4,
            },
            virtio: (0..3)
                .map(|i| IoRegion {
                    space: IoSpace::Mmio,
                    address: 0x10000 + i * 0x2000,
                    size: 0x1000,
                    irq: 10 + i as u32,
                })
                .collect(),
            hotplug: Some(MemoryRange {
                address: 0x8000_0000,
                size: h.capacity,
            }),
        };
        match self.fault {
            Fault::Overlap => l.virtio[0].address = self.base,
            Fault::Overflow => l.ram[0].address = u64::MAX - 16,
            Fault::Capacity => l.ram.pop().map(|_| ()).unwrap(),
            Fault::DeviceCount => {
                l.virtio.pop();
            }
            Fault::Hotplug => l.hotplug.as_mut().unwrap().address += 1,
            _ => (),
        }
        Ok(l)
    }

    fn create(&self, _: &VmConfig) -> Result<TestVm, PlatformError> {
        if self.fault == Fault::Create {
            return Err(PlatformError::Backend(
                io::Error::other("create failure").into(),
            ));
        }
        self.state.borrow_mut().log.push("create".into());
        Ok(TestVm {
            state: self.state.clone(),
            fault: self.fault,
            events: VecDeque::new(),
            memory: None,
            mapped: BTreeMap::new(),
            stopped: false,
            paused: false,
            base: self.base,
        })
    }
}

struct TestVm {
    state: Shared,
    fault: Fault,
    events: VecDeque<Event<usize>>,
    memory: Option<GuestMemoryMmap>,
    /// Owned until a successful unmap or destruction, like a real hypervisor VM.
    mapped: BTreeMap<u64, Arc<GuestRegionMmap>>,
    stopped: bool,
    paused: bool,
    base: u64,
}

impl Mapper for TestVm {
    fn map(&mut self, r: Arc<GuestRegionMmap>) -> Result<(), MemoryError> {
        let mut s = self.state.borrow_mut();
        s.allocations
            .push((r.start_addr().0, Arc::downgrade(&r.get_mmap())));
        s.calls += 1;
        s.log.push(format!("map:{:x}", r.start_addr().0));
        if r.start_addr().0 >= 0x8000_0000 {
            assert!(self.paused);
        }
        if self.fault == Fault::BaseMap && s.calls == 2 {
            return Err(MemoryError::Backend(
                io::Error::other("base mapping failure").into(),
            ));
        }
        if matches!(self.fault, Fault::Map | Fault::Rollback) && s.calls == 4 {
            return Err(MemoryError::Backend(
                io::Error::other("dynamic mapping failure").into(),
            ));
        }
        assert!(self.mapped.insert(r.start_addr().0, r).is_none());
        Ok(())
    }

    fn unmap(&mut self, r: &GuestRegionMmap) -> Result<(), MemoryError> {
        self.state
            .borrow_mut()
            .log
            .push(format!("unmap:{:x}", r.start_addr().0));
        assert!(self.stopped || self.paused);
        let mapped = self
            .mapped
            .get(&r.start_addr().0)
            .ok_or_else(|| MemoryError::Backend(io::Error::other("region is not mapped").into()))?;
        assert_eq!(mapped.len(), r.len());
        assert!(Arc::ptr_eq(&mapped.get_mmap(), &r.get_mmap()));
        // Failures below keep the allocation in the table.
        if matches!(self.fault, Fault::Unmap | Fault::Precedence) {
            return Err(MemoryError::Backend(
                io::Error::other("unmap failure").into(),
            ));
        }
        if self.fault == Fault::Rollback && !self.stopped {
            return Err(MemoryError::Backend(
                io::Error::other("rollback failure").into(),
            ));
        }
        self.mapped.remove(&r.start_addr().0);
        Ok(())
    }
}

impl VirtualMachine for TestVm {
    type Completion = usize;

    fn prepare(
        &mut self,
        memory: &GuestMemoryMmap,
        layout: &MachineLayout,
    ) -> Result<(), PlatformError> {
        self.state.borrow_mut().log.push("prepare".into());
        self.memory = Some(memory.clone());
        if self.fault == Fault::Prepare {
            return Err(PlatformError::Backend(
                io::Error::other("prepare failure").into(),
            ));
        }
        assert_eq!(memory.num_regions(), 2);
        // Populate a real split virtqueue with a two-block plug request.
        let b = self.base;
        let descriptor = |address: u64, len: u32, flags: u16, next: u16| {
            let mut d = [0u8; 16];
            d[..8].copy_from_slice(&address.to_le_bytes());
            d[8..12].copy_from_slice(&len.to_le_bytes());
            d[12..14].copy_from_slice(&flags.to_le_bytes());
            d[14..].copy_from_slice(&next.to_le_bytes());
            d
        };
        memory
            .write_slice(&descriptor(b + 0x8000, 24, 1, 1), GuestAddress(b + 0x1000))
            .map_err(|e| PlatformError::Backend(Box::new(e)))?;
        memory
            .write_slice(
                &descriptor(
                    b + 0x9000,
                    if self.fault == Fault::MemoryRequest {
                        4
                    } else {
                        10
                    },
                    2,
                    0,
                ),
                GuestAddress(b + 0x1010),
            )
            .map_err(|e| PlatformError::Backend(Box::new(e)))?;
        let mut req = [0u8; 24];
        req[8..16].copy_from_slice(&layout.hotplug.unwrap().address.to_le_bytes());
        req[16..18].copy_from_slice(&2u16.to_le_bytes());
        memory
            .write_slice(&req, GuestAddress(b + 0x8000))
            .map_err(|e| PlatformError::Backend(Box::new(e)))?;
        memory
            .write_obj(1u16, GuestAddress(b + 0x2002))
            .map_err(|e| PlatformError::Backend(Box::new(e)))?;
        let mut io = |space, address, width, write, value| {
            let completion = self.events.len();
            self.events.push_back(Event::Io(IoAccess {
                space,
                address,
                width,
                write,
                value,
                completion,
            }));
            completion
        };
        let mut expect_reply = Vec::new();
        let mut expect_irq = Vec::new();
        let virtio = |device: usize, offset: u64| layout.virtio[device].address + offset;
        // 32-bit register writes; disk "a" is device 0 and "z" device 1.
        let registers = |io: &mut dyn FnMut(IoSpace, u64, usize, bool, u64) -> usize,
                         device: usize,
                         writes: &[(u64, u64)]| {
            for &(offset, value) in writes {
                io(IoSpace::Mmio, virtio(device, offset), 4, true, value);
            }
        };
        let queue = |base: u64| {
            [
                (0x70, 1),
                (0x70, 3),
                (0x24, 1),
                (0x20, 1),
                (0x24, 0),
                (0x20, 0),
                (0x70, 11),
                (0x30, 0),
                (0x38, 8),
                (0x80, base),
                (0x90, base + 0x1000),
                (0xa0, base + 0x2000),
                (0x44, 1),
            ]
        };
        io(layout.uart.space, layout.uart.address, 1, true, b'X' as u64);
        io(layout.uart.space, layout.uart.address + 5, 1, false, 0);
        io(IoSpace::Mmio, layout.virtio[0].address, 4, false, 0);
        if self.fault == Fault::GuestErrors {
            // A ready queue on "z" whose available index overruns the ring: the
            // memory transaction's pin scan finds it before DRIVER_OK.
            for (base, avail) in [(b + 0x10000, 9u16), (b + 0x20000, 9)] {
                memory
                    .write_obj(avail, GuestAddress(base + 0x1002))
                    .map_err(|e| PlatformError::Backend(Box::new(e)))?;
            }
            registers(&mut io, 1, &queue(b + 0x10000));
        }
        for (offset, value) in [
            (0x70, 1),
            (0x70, 3),
            (0x24, 1),
            (0x20, 1),
            (0x24, 0),
            (0x20, 2),
            (0x70, 11),
            (0x30, 0),
            (0x38, 8),
            (0x80, b + 0x1000),
            (0x90, b + 0x2000),
            (0xa0, b + 0x3000),
            (0x44, 1),
            (0x70, 15),
        ] {
            io(
                IoSpace::Mmio,
                layout.virtio[2].address + offset,
                4,
                true,
                value,
            );
        }
        // Each completion follows the level change of the access it completes.
        let c = io(layout.uart.space, layout.uart.address + 1, 1, true, 2);
        expect_irq.push((c, layout.uart.irq, true)); // IER enables THRE.
        let c = io(layout.uart.space, layout.uart.address + 2, 1, false, 0);
        expect_irq.push((c, layout.uart.irq, false)); // Reading IIR clears it.
        expect_reply.push((c, 0xc2));
        let c = io(IoSpace::Mmio, virtio(2, 0x64), 4, true, 3);
        expect_irq.push((c, layout.virtio[2].irq, false)); // InterruptACK.
        if self.fault == Fault::GuestErrors {
            let mmio = IoSpace::Mmio;
            let (a, irq) = (0, layout.virtio[0].irq);
            let mut read = |io: &mut dyn FnMut(IoSpace, u64, usize, bool, u64) -> usize,
                            address,
                            width,
                            value| {
                expect_reply.push((io(mmio, address, width, false, 0), value));
            };
            // Undecodable accesses inside device windows.
            read(&mut io, layout.uart.address, 2, 0xffff);
            io(mmio, virtio(a, 0x70), 2, true, 0xf);
            read(&mut io, virtio(a, 0x70), 2, 0xffff);
            read(&mut io, virtio(a, 0), 3, 0xff_ffff);
            // Rejected register writes change nothing.
            registers(&mut io, a, &[(0x70, 3), (0x70, 1)]);
            read(&mut io, virtio(a, 0x70), 4, 3);
            registers(&mut io, a, &[(0x24, 1), (0x20, 1), (0x24, 0), (0x70, 15)]);
            read(&mut io, virtio(a, 0x70), 4, 3);
            let mut init = queue(b + 0x20000).to_vec();
            init.push((0x70, 15));
            registers(&mut io, a, &init[2..]);
            read(&mut io, virtio(a, 0x70), 4, 15);
            // A corrupt queue isolates "a" and raises a configuration change.
            let c = io(mmio, virtio(a, 0x50), 4, true, 0);
            expect_irq.push((c, irq, true));
            read(&mut io, virtio(a, 0x70), 4, 0x4f);
            read(&mut io, virtio(a, 0x60), 4, 2);
            // "z" was isolated before DRIVER_OK, so it raised no interrupt.
            read(&mut io, virtio(1, 0x70), 4, 0x4b);
            read(&mut io, virtio(1, 0x60), 4, 0);
            // Reset recovers the device.
            let c = io(mmio, virtio(a, 0x70), 4, true, 0);
            expect_irq.push((c, irq, false));
            read(&mut io, virtio(a, 0x70), 4, 0);
        }
        {
            let mut s = self.state.borrow_mut();
            s.expect_reply = expect_reply;
            s.expect_irq = expect_irq;
        }
        if self.fault == Fault::Panic {
            // Runs after virtio-mem has plugged dynamic RAM; the serial backend panics on it.
            io(layout.uart.space, layout.uart.address, 1, true, b'P' as u64);
        }
        self.events.push_back(Event::Shutdown);
        Ok(())
    }

    fn start(&mut self) -> Result<(), PlatformError> {
        self.state.borrow_mut().log.push("start".into());
        if self.fault == Fault::Start {
            return Err(PlatformError::Backend(
                io::Error::other("start failure").into(),
            ));
        }
        Ok(())
    }

    fn poll_event(&mut self, _: Duration) -> Result<Option<Event<usize>>, PlatformError> {
        if matches!(self.fault, Fault::Poll | Fault::Precedence) {
            return Err(PlatformError::Backend(
                io::Error::other("poll failure").into(),
            ));
        }
        Ok(self.events.pop_front())
    }

    fn complete_io(&mut self, completion: usize, value: u64) -> Result<(), PlatformError> {
        let mut s = self.state.borrow_mut();
        assert_eq!(completion, s.replies.len());
        s.replies.push(value);
        s.trace.push(format!("reply:{completion}"));
        Ok(())
    }

    fn set_irq(&mut self, irq: u32, level: bool) -> Result<(), PlatformError> {
        let mut s = self.state.borrow_mut();
        s.irqs.push((irq, level));
        s.trace.push(format!("irq:{irq}:{level}"));
        Ok(())
    }

    fn pause(&mut self) -> Result<PauseOutcome, PlatformError> {
        self.state.borrow_mut().log.push("pause".into());
        match self.fault {
            Fault::Pause => {
                return Err(PlatformError::Backend(
                    io::Error::other("pause failure").into(),
                ));
            }
            Fault::PauseStopping | Fault::PauseVcpuError => return Ok(PauseOutcome::Stopping),
            _ => (),
        }
        self.paused = true;
        Ok(PauseOutcome::Paused)
    }

    fn resume(&mut self) -> Result<(), PlatformError> {
        self.state.borrow_mut().log.push("resume".into());
        if self.fault == Fault::Resume {
            return Err(PlatformError::Backend(
                io::Error::other("resume failure").into(),
            ));
        }
        self.paused = false;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlatformError> {
        if !self.stopped {
            self.state.borrow_mut().log.push("stop".into());
            self.stopped = true;
            // A pending vCPU fault is reported once; repeated stops succeed.
            if self.fault.vcpu_fault() {
                return Err(PlatformError::Backend(
                    io::Error::other("vcpu fault").into(),
                ));
            }
        }
        Ok(())
    }
}

impl Drop for TestVm {
    fn drop(&mut self) {
        let stopped = self.stopped;
        // Unwinding reaches here without cleanup; a fault would be reported now.
        let _ = self.stop();
        assert!(stopped || std::thread::panicking());
        // Base RAM must still be alive and readable during VM destruction.
        if let Some(m) = &self.memory {
            assert_eq!(m.read_obj::<u8>(GuestAddress(self.base)).unwrap(), 0);
        }
        let mut s = self.state.borrow_mut();
        s.released = s
            .allocations
            .iter()
            .filter(|(_, weak)| weak.upgrade().is_none())
            .map(|(address, _)| *address)
            .collect();
        for address in self.mapped.keys() {
            s.log.push(format!("retained:{address:x}"));
        }
        s.log.push("destroy".into());
        // Dropping the table releases the allocations this VM still owned.
    }
}

struct Disk(Shared, bool);

impl BlockStorage for Disk {
    fn size(&self) -> u64 {
        512
    }

    fn read_only(&self) -> bool {
        false
    }

    fn read(&self, _: u64, d: &[VolatileSlice<'_>]) -> Result<(), StorageError> {
        for s in d {
            s.copy_from(&vec![0u8; s.len()]);
        }
        Ok(())
    }

    fn write(&self, _: u64, _: &[VolatileSlice<'_>]) -> Result<(), StorageError> {
        Ok(())
    }

    fn flush(&self) -> Result<(), StorageError> {
        self.0.borrow_mut().log.push("flush".into());
        if self.1 {
            return Err(StorageError::Backend(
                io::Error::other("flush failure").into(),
            ));
        }
        Ok(())
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        self.0.borrow_mut().log.push("disk-drop".into());
    }
}

struct Serial(Shared);

impl io::Write for Serial {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if b == b"P" {
            panic!("injected serial backend panic");
        }
        self.0.borrow_mut().output.extend(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SerialIo for Serial {
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

    fn send(&mut self, _: &[VolatileSlice<'_>]) -> Result<bool, NetError> {
        Ok(true)
    }

    fn recv(&mut self, _: &[VolatileSlice<'_>]) -> Result<Option<usize>, NetError> {
        Ok(None)
    }
}

fn run(fault: Fault, port: bool, base: u64, strict: bool) -> Shared {
    let state = Shared::default();
    let memory = VirtioMem::new(128, 2).unwrap();
    let blocks = [
        ("z".into(), Disk(state.clone(), fault.flush_fails())),
        ("a".into(), Disk(state.clone(), fault.flush_fails())),
    ]
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    let vm = Vmm::new(
        VmConfig {
            memory_mib: 2,
            vcpu_count: 1,
            strict,
        },
        blocks,
        None::<NoNet>,
        Some(memory),
        Serial(state.clone()),
    );
    let control = vm.control();
    control.set_requested_mib(128).unwrap();
    let result = vm.run_with_platform(TestPlatform {
        state: state.clone(),
        fault,
        port,
        base,
    });
    // Guest errors are contained in lenient mode; host failures never are.
    let contained = matches!(fault, Fault::GuestErrors | Fault::MemoryRequest) && !strict;
    let completes = matches!(fault, Fault::None | Fault::Map | Fault::PauseStopping) || contained;
    assert_eq!(result.is_ok(), completes, "{fault:?}: {result:?}");
    // VmControl::stop reports cleanup failures only, never execution faults.
    let cleanup = control.stop();
    assert_eq!(
        cleanup.is_err(),
        fault.flush_fails() || fault == Fault::Unmap,
        "{fault:?}: {cleanup:?}"
    );
    if fault.flush_fails() {
        assert_eq!(cleanup.unwrap_err().to_string(), "flush failure");
    }
    let backend = |e: &PlatformError| match e {
        PlatformError::Backend(e) => e.to_string(),
        e => panic!("unexpected {e}"),
    };
    use w_vmm::{Error, error::DeviceError};
    match (fault, result.as_ref().err()) {
        (
            Fault::None
            | Fault::Map
            | Fault::PauseStopping
            | Fault::GuestErrors
            | Fault::MemoryRequest,
            None,
        ) => (),
        (
            Fault::MemoryRequest,
            Some(Error::Device(DeviceError::Memory(MemoryError::InvalidVirtioMemRequestBuffers {
                read: 24,
                write: 4,
            }))),
        ) => assert!(strict),
        (
            Fault::GuestErrors,
            Some(Error::Device(DeviceError::AvailableRingOverrun { count: 9, size: 8 })),
        ) => assert!(strict),
        (Fault::PauseVcpuError | Fault::StopVcpuError, Some(Error::Platform(e))) => {
            assert_eq!(backend(e), "vcpu fault");
        }
        (Fault::Precedence, Some(Error::Platform(e))) => assert_eq!(backend(e), "poll failure"),
        (Fault::Overlap, Some(Error::Platform(PlatformError::OverlappingRegions { .. }))) => (),
        (Fault::Overflow, Some(Error::Platform(PlatformError::RegionOverflow { .. }))) => (),
        (Fault::Capacity, Some(Error::Platform(PlatformError::RamCapacityMismatch { .. }))) => (),
        (Fault::DeviceCount, Some(Error::Platform(PlatformError::DeviceCountMismatch { .. }))) => {}
        (
            Fault::Hotplug,
            Some(Error::Platform(PlatformError::InvalidHotplugCapacityAlignment { .. })),
        ) => {}
        (
            Fault::Create
            | Fault::Prepare
            | Fault::Start
            | Fault::Pause
            | Fault::Poll
            | Fault::Resume,
            Some(Error::Platform(PlatformError::Backend(_))),
        ) => (),
        (Fault::BaseMap | Fault::Unmap, Some(Error::Memory(MemoryError::Backend(_)))) => (),
        (
            Fault::Rollback,
            Some(Error::Device(DeviceError::Memory(MemoryError::RollbackFailed {
                source,
                rollback,
            }))),
        ) => {
            assert!(matches!(source.as_ref(), MemoryError::Backend(_)));
            assert!(matches!(rollback.as_ref(), MemoryError::Backend(_)));
        }
        (Fault::Flush, Some(Error::Device(DeviceError::Storage(StorageError::Backend(_))))) => (),
        _ => panic!("unexpected error for {fault:?}: {result:?}"),
    }
    assert_eq!(
        control.memory_status().unwrap().lifecycle,
        MemoryLifecycle::Stopped
    );
    assert!(!control.memory_status().unwrap().driver_ready);
    assert!(control.set_requested_mib(0).is_err());
    let s = state.borrow();
    assert!(
        s.allocations.iter().all(|(_, w)| w.upgrade().is_none()),
        "allocation leaked after VM destruction"
    );
    // The VM owns failed unmaps and rollback leftovers until its destruction.
    let mut retained: Vec<_> = s
        .log
        .iter()
        .filter_map(|e| e.strip_prefix("retained:"))
        .map(|a| u64::from_str_radix(a, 16).unwrap())
        .collect();
    retained.sort();
    let ram = [base, base + (2 << 20)];
    let expected = match fault {
        Fault::Unmap => vec![ram[0], ram[1], 0x8000_0000, 0x8020_0000],
        Fault::Precedence => ram.to_vec(),
        // The first block stays mapped after its rollback unmap fails.
        Fault::Rollback => vec![0x8000_0000],
        _ => vec![],
    };
    assert_eq!(retained, expected, "{fault:?}");
    if fault == Fault::Rollback {
        assert!(s.released.contains(&0x8020_0000));
        assert!(!s.released.contains(&0x8000_0000));
    }
    if let Some(stop) = s.log.iter().position(|s| s == "stop") {
        // A guest device reset also flushes its disk before stop.
        let flush = stop + s.log[stop..].iter().position(|s| s == "flush").unwrap();
        let destroy = s.log.iter().position(|s| s == "destroy").unwrap();
        assert!(flush < destroy);
        assert_eq!(s.log[stop..].iter().filter(|s| *s == "flush").count(), 2);
        for (i, e) in s.log.iter().enumerate() {
            if e.starts_with("unmap:") && i > stop {
                assert!(i > flush && i < destroy);
            }
            if e == "disk-drop" {
                assert!(i > destroy);
            }
        }
    }
    if matches!(
        fault,
        Fault::None | Fault::Map | Fault::Rollback | Fault::Resume
    ) || (fault == Fault::GuestErrors && !strict)
    {
        assert!(s.log.contains(&"map:80000000".into()));
        assert!(s.log.contains(&"map:80200000".into()));
        assert_eq!(s.output, b"X");
        assert_eq!(s.replies[2], 0x74726976);
        assert_eq!(s.log.contains(&"resume".into()), fault != Fault::Rollback);
        if matches!(fault, Fault::None | Fault::GuestErrors) {
            assert!(s.irqs.contains(&(12, true)));
            assert_eq!(control.memory_status().unwrap().plugged_size_mib, 4);
        }
    }
    if matches!(fault, Fault::None | Fault::Map) || contained {
        for &(completion, value) in &s.expect_reply {
            assert_eq!(
                s.replies[completion], value,
                "{fault:?}: reply {completion}"
            );
        }
        for &(completion, irq, level) in &s.expect_irq {
            let reply = format!("reply:{completion}");
            let at = s.trace.iter().position(|e| *e == reply).unwrap();
            assert_eq!(s.trace[at - 1], format!("irq:{irq}:{level}"), "{fault:?}");
        }
    }
    if fault == Fault::MemoryRequest {
        // The isolated device maps nothing; lenient mode still resumes the vCPUs.
        assert!(!s.log.iter().any(|e| e.starts_with("map:8")));
        assert_eq!(s.log.contains(&"resume".into()), !strict);
        assert_eq!(control.memory_status().unwrap().plugged_size_mib, 0);
        assert_eq!(s.output, b"X");
    }
    if fault == Fault::GuestErrors && strict {
        // The pin scan stops the VM before any dynamic mapping.
        assert!(!s.log.iter().any(|e| e.starts_with("map:8")));
        assert!(!s.log.contains(&"resume".into()));
    }
    if matches!(
        fault,
        Fault::Pause | Fault::PauseStopping | Fault::PauseVcpuError
    ) {
        assert!(s.log.contains(&"pause".into()));
        assert!(!s.log.contains(&"map:80000000".into()));
        // A transaction that observes Stopping terminates and never resumes.
        assert!(!s.log.contains(&"resume".into()));
        assert_eq!(control.memory_status().unwrap().plugged_size_mib, 0);
    }
    if fault == Fault::StopVcpuError {
        // Normal guest shutdown still ran every event before stop.
        assert!(s.log.contains(&"map:80200000".into()));
        assert_eq!(s.output, b"X");
    }
    drop(s);
    state
}

#[test]
fn external_platform_mmio_and_port_io_multiple_ram_ranges() {
    for port in [false, true] {
        for base in [0x100000, 0x10000000] {
            for strict in [true, false] {
                run(Fault::None, port, base, strict);
            }
        }
    }
}

#[test]
fn errors_and_transaction_rollback_cleanup() {
    for fault in [
        Fault::Overlap,
        Fault::Overflow,
        Fault::Capacity,
        Fault::DeviceCount,
        Fault::Hotplug,
        Fault::Create,
        Fault::Prepare,
        Fault::Start,
        Fault::Pause,
        Fault::BaseMap,
        Fault::Map,
        Fault::Rollback,
        Fault::Poll,
        Fault::Resume,
        Fault::Flush,
        Fault::Unmap,
    ] {
        // Host failures, including rollback failure, are fatal in both modes.
        for strict in [true, false] {
            run(fault, false, 0x100000, strict);
        }
    }
}

#[test]
fn stopping_races_and_error_precedence() {
    for fault in [
        Fault::PauseStopping,
        Fault::PauseVcpuError,
        Fault::StopVcpuError,
        Fault::Precedence,
    ] {
        run(fault, false, 0x100000, true);
    }
}

#[test]
fn strict_guest_errors_stop_and_lenient_guest_errors_isolate() {
    for fault in [Fault::GuestErrors, Fault::MemoryRequest] {
        for strict in [true, false] {
            run(fault, false, 0x100000, strict);
        }
    }
}

#[test]
fn dropping_unused_memory_stops_control() {
    let m = VirtioMem::new(128, 2).unwrap();
    let vm = Vmm::new(
        VmConfig::default(),
        BTreeMap::<String, Disk>::new(),
        None::<NoNet>,
        Some(m),
        Serial(Shared::default()),
    );
    let c = vm.control();
    drop(vm);
    assert_eq!(
        c.memory_status().unwrap().lifecycle,
        MemoryLifecycle::Stopped
    );
}

#[test]
fn backend_panic_destroys_vm_before_releasing_ram() {
    let state = Shared::default();
    let blocks = [
        ("z".into(), Disk(state.clone(), false)),
        ("a".into(), Disk(state.clone(), false)),
    ]
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    let vm = Vmm::new(
        VmConfig {
            memory_mib: 2,
            ..VmConfig::default()
        },
        blocks,
        None::<NoNet>,
        Some(VirtioMem::new(128, 2).unwrap()),
        Serial(state.clone()),
    );
    vm.control().set_requested_mib(128).unwrap();
    let platform = TestPlatform {
        state: state.clone(),
        fault: Fault::Panic,
        port: false,
        base: 0x100000,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.run_with_platform(platform)
    }));
    let message = result.unwrap_err();
    assert_eq!(
        message.downcast_ref::<&str>(),
        Some(&"injected serial backend panic")
    );
    let s = state.borrow();
    // Dynamic RAM was plugged before the panic, and no cleanup ran.
    assert!(s.log.contains(&"map:80000000".into()));
    assert!(
        !s.log
            .iter()
            .any(|e| e == "flush" || e.starts_with("unmap:"))
    );
    // Unwinding destroys the VM while it still owns every mapping it made.
    assert!(s.released.is_empty(), "{:?}", s.released);
    for address in ["100000", "300000", "80000000", "80200000"] {
        assert!(
            s.log.contains(&format!("retained:{address}")),
            "{:?}",
            s.log
        );
    }
    let stop = s.log.iter().position(|e| e == "stop").unwrap();
    let destroy = s.log.iter().position(|e| e == "destroy").unwrap();
    let drops: Vec<_> = (0..s.log.len())
        .filter(|&i| s.log[i] == "disk-drop")
        .collect();
    assert!(stop < destroy);
    assert_eq!(drops.len(), 2);
    assert!(drops.iter().all(|&i| i > destroy), "{:?}", s.log);
    assert!(
        s.allocations.iter().all(|(_, w)| w.upgrade().is_none()),
        "allocation leaked after unwinding"
    );
}
