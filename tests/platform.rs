//! External platform exercising the real public runtime, without private devices.
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    io,
    rc::Rc,
    time::Duration,
};
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
}

#[derive(Default)]
struct State {
    allocations: Vec<(u64, std::sync::Weak<vm_memory::MmapRegion>)>,
    log: Vec<String>,
    calls: usize,
    replies: Vec<u64>,
    output: Vec<u8>,
    irqs: Vec<(u32, bool)>,
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
    stopped: bool,
    paused: bool,
    base: u64,
}

impl Mapper for TestVm {
    fn map(&mut self, r: &GuestRegionMmap) -> Result<(), MemoryError> {
        let mut s = self.state.borrow_mut();
        s.allocations
            .push((r.start_addr().0, std::sync::Arc::downgrade(&r.get_mmap())));
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
        Ok(())
    }

    fn unmap(&mut self, r: &GuestRegionMmap) -> Result<(), MemoryError> {
        self.state
            .borrow_mut()
            .log
            .push(format!("unmap:{:x}", r.start_addr().0));
        assert!(self.stopped || self.paused);
        if self.fault == Fault::Unmap {
            return Err(MemoryError::Backend(
                io::Error::other("unmap failure").into(),
            ));
        }
        if self.fault == Fault::Rollback && !self.stopped {
            return Err(MemoryError::Backend(
                io::Error::other("rollback failure").into(),
            ));
        }
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
            .write_slice(&descriptor(b + 0x9000, 10, 2, 0), GuestAddress(b + 0x1010))
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
        };
        io(layout.uart.space, layout.uart.address, 1, true, b'X' as u64);
        io(layout.uart.space, layout.uart.address + 5, 1, false, 0);
        io(IoSpace::Mmio, layout.virtio[0].address, 4, false, 0);
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

    fn poll_event(
        &mut self,
        _: Duration,
    ) -> Result<Option<Event<usize>>, PlatformError> {
        if self.fault == Fault::Poll {
            return Err(PlatformError::Backend(
                io::Error::other("poll failure").into(),
            ));
        }
        Ok(self.events.pop_front())
    }

    fn complete_io(
        &mut self,
        completion: usize,
        value: u64,
    ) -> Result<(), PlatformError> {
        let mut s = self.state.borrow_mut();
        assert_eq!(completion, s.replies.len());
        s.replies.push(value);
        Ok(())
    }

    fn set_irq(&mut self, irq: u32, level: bool) -> Result<(), PlatformError> {
        self.state.borrow_mut().irqs.push((irq, level));
        Ok(())
    }

    fn pause(&mut self) -> Result<(), PlatformError> {
        self.state.borrow_mut().log.push("pause".into());
        if self.fault == Fault::Pause {
            return Err(PlatformError::Backend(
                io::Error::other("pause failure").into(),
            ));
        }
        self.paused = true;
        Ok(())
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

    fn stop(&mut self) {
        if !self.stopped {
            self.state.borrow_mut().log.push("stop".into());
            self.stopped = true;
        }
    }
}

impl Drop for TestVm {
    fn drop(&mut self) {
        self.stop();
        // Base RAM must still be alive and readable during VM destruction.
        if let Some(m) = &self.memory {
            assert_eq!(m.read_obj::<u8>(GuestAddress(self.base)).unwrap(), 0);
        }
        for (address, weak) in &self.state.borrow().allocations {
            if *address < 0x8000_0000
                || matches!(
                    self.fault,
                    Fault::None | Fault::Rollback | Fault::Resume | Fault::Flush | Fault::Unmap
                )
            {
                assert!(
                    weak.upgrade().is_some(),
                    "allocation released before VM destruction"
                );
            }
        }
        self.state.borrow_mut().log.push("destroy".into());
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

    fn read(&self, _: u64, d: &mut [u8]) -> Result<(), StorageError> {
        d.fill(0);
        Ok(())
    }

    fn write(&self, _: u64, _: &[u8]) -> Result<(), StorageError> {
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

    fn send(&mut self, _: &[u8]) -> Result<bool, NetError> {
        Ok(true)
    }

    fn recv(&mut self, _: &mut [u8]) -> Result<Option<usize>, NetError> {
        Ok(None)
    }
}

fn run(fault: Fault, port: bool, base: u64) -> Shared {
    let state = Shared::default();
    let memory = VirtioMem::new(128).unwrap();
    let blocks = [
        ("z".into(), Disk(state.clone(), fault == Fault::Flush)),
        ("a".into(), Disk(state.clone(), fault == Fault::Flush)),
    ]
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    let vm = Vmm::new(
        VmConfig {
            memory_mib: 2,
            vcpu_count: 1,
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
    assert_eq!(
        result.is_ok(),
        matches!(fault, Fault::None | Fault::Map),
        "{fault:?}: {result:?}"
    );
    use w_vmm::{Error, error::DeviceError};
    match (fault, result.as_ref().err()) {
        (Fault::None | Fault::Map, None) => (),
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
    if let Some(stop) = s.log.iter().position(|s| s == "stop") {
        let flush = s.log.iter().position(|s| s == "flush").unwrap();
        let destroy = s.log.iter().position(|s| s == "destroy").unwrap();
        assert!(stop < flush && flush < destroy);
        assert_eq!(s.log.iter().filter(|s| *s == "flush").count(), 2);
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
    ) {
        assert!(s.log.contains(&"map:80000000".into()));
        assert!(s.log.contains(&"map:80200000".into()));
        assert_eq!(s.output, b"X");
        assert_eq!(s.replies[2], 0x74726976);
        assert_eq!(s.log.contains(&"resume".into()), fault != Fault::Rollback);
        if fault == Fault::None {
            assert!(s.irqs.contains(&(12, true)));
            assert_eq!(control.memory_status().unwrap().plugged_size_mib, 4);
        }
    }
    if fault == Fault::Pause {
        assert!(!s.log.contains(&"map:80000000".into()));
    }
    drop(s);
    state
}

#[test]
fn external_platform_mmio_and_port_io_multiple_ram_ranges() {
    for port in [false, true] {
        for base in [0x100000, 0x10000000] {
            run(Fault::None, port, base);
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
        run(fault, false, 0x100000);
    }
}

#[test]
fn dropping_unused_memory_stops_control() {
    let m = VirtioMem::new(128).unwrap();
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
