//! Exercise synchronous control through the public runtime with !Send backends.
use std::{
    collections::BTreeMap,
    io,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use vm_memory::VolatileSlice;
use vm_memory::{GuestMemoryMmap, GuestRegionMmap};
use w_vmm::Result;
use w_vmm::error::{MemoryError, NetError, PlatformError, StorageError};
use w_vmm::{
    memory::Mapper, net::NetDevice, platform::*, serial::SerialIo, storage::BlockStorage, *,
};

#[derive(Default)]
struct State {
    ticks: AtomicUsize,
    output: AtomicUsize,
    reads: AtomicUsize,
    log: Mutex<Vec<&'static str>>,
}

impl State {
    fn log(&self, s: &'static str) {
        self.log.lock().unwrap().push(s);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    Prepare,
    Pause,
    Resume,
    Flush,
}

struct Machine {
    state: Arc<State>,
    fault: Fault,
    control: VmControl,
    _local: Rc<()>,
}

impl Platform for Machine {
    type Vm = Self;

    fn layout(
        &self,
        _: &VmConfig,
        needs: &DeviceRequirements,
    ) -> std::result::Result<MachineLayout, PlatformError> {
        if let Some(hotplug) = &needs.hotplug {
            let status = self.control.memory_status().unwrap();
            assert_eq!(hotplug.capacity, status.region_size_mib << 20);
            assert_eq!(hotplug.alignment, status.block_size_mib.max(128) << 20);
        }
        Ok(MachineLayout {
            ram: vec![MemoryRange {
                address: 0x100000,
                size: 1 << 20,
            }],
            uart: IoRegion {
                space: IoSpace::Mmio,
                address: 0x1000,
                size: 8,
                irq: 1,
            },
            virtio: needs
                .devices
                .iter()
                .enumerate()
                .map(|(i, _)| IoRegion {
                    space: IoSpace::Mmio,
                    address: 0x10000 + i as u64 * 0x1000,
                    size: 0x1000,
                    irq: 2 + i as u32,
                })
                .collect(),
            hotplug: needs.hotplug.as_ref().map(|m| MemoryRange {
                address: 0x80000000,
                size: m.capacity,
            }),
        })
    }

    fn create(&self, _: &VmConfig) -> std::result::Result<Self, PlatformError> {
        Ok(Self {
            state: self.state.clone(),
            fault: self.fault,
            control: self.control.clone(),
            _local: self._local.clone(),
        })
    }
}

impl Mapper for Machine {
    fn map(&mut self, _: &GuestRegionMmap) -> std::result::Result<(), MemoryError> {
        Ok(())
    }

    fn unmap(&mut self, _: &GuestRegionMmap) -> std::result::Result<(), MemoryError> {
        self.state.log("unmap");
        Ok(())
    }
}

impl VirtualMachine for Machine {
    type Completion = ();

    fn prepare(
        &mut self,
        _: &GuestMemoryMmap,
        _: &MachineLayout,
    ) -> std::result::Result<(), PlatformError> {
        if self.fault == Fault::Prepare {
            return Err(PlatformError::Backend(
                io::Error::other("prepare failed").into(),
            ));
        }
        Ok(())
    }

    fn start(&mut self) -> std::result::Result<(), PlatformError> {
        Ok(())
    }

    fn poll_event(&mut self, _: Duration) -> std::result::Result<Option<Event<()>>, PlatformError> {
        self.state.ticks.fetch_add(1, Ordering::SeqCst);
        // A callback must fail immediately, never wait on its own runtime.
        assert!(
            self.control
                .pause()
                .unwrap_err()
                .to_string()
                .contains("VMM thread")
        );
        assert!(self.control.resume().is_err());
        assert!(self.control.stop().is_err());
        Ok(Some(Event::Io(IoAccess {
            space: IoSpace::Mmio,
            address: 0x1000,
            width: 1,
            write: true,
            value: b'x' as u64,
            completion: (),
        })))
    }

    fn complete_io(&mut self, _: (), _: u64) -> std::result::Result<(), PlatformError> {
        Ok(())
    }

    fn set_irq(&mut self, _: u32, _: bool) -> std::result::Result<(), PlatformError> {
        Ok(())
    }

    fn pause(&mut self) -> std::result::Result<(), PlatformError> {
        self.state.log("pause");
        if self.fault == Fault::Pause {
            return Err(PlatformError::Backend(
                io::Error::other("pause failed").into(),
            ));
        }
        Ok(())
    }

    fn resume(&mut self) -> std::result::Result<(), PlatformError> {
        self.state.log("resume");
        if self.fault == Fault::Resume {
            return Err(PlatformError::Backend(
                io::Error::other("resume failed").into(),
            ));
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.state.log("stop");
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.state.log("destroy");
    }
}

struct Disk(Arc<State>, Fault);

impl BlockStorage for Disk {
    fn size(&self) -> u64 {
        512
    }

    fn read_only(&self) -> bool {
        false
    }

    fn read(&self, _: u64, _: &[VolatileSlice<'_>]) -> std::result::Result<(), StorageError> {
        Ok(())
    }

    fn write(&self, _: u64, _: &[VolatileSlice<'_>]) -> std::result::Result<(), StorageError> {
        Ok(())
    }

    fn flush(&self) -> std::result::Result<(), StorageError> {
        self.0.log("flush");
        if self.1 == Fault::Flush {
            return Err(StorageError::Backend(
                io::Error::other("flush failed").into(),
            ));
        }
        Ok(())
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        self.0.log("disk-drop");
    }
}

struct Serial(Arc<State>);

impl io::Write for Serial {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.output.fetch_add(b.len(), Ordering::SeqCst);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SerialIo for Serial {
    fn recv(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.0.reads.fetch_add(1, Ordering::SeqCst);
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

    fn send(&mut self, _: &[VolatileSlice<'_>]) -> std::result::Result<bool, NetError> {
        Ok(true)
    }

    fn recv(&mut self, _: &[VolatileSlice<'_>]) -> std::result::Result<Option<usize>, NetError> {
        Ok(None)
    }
}

fn new_vm(state: Arc<State>, fault: Fault, memory: Option<VirtioMem>) -> Vmm<Disk, NoNet, Serial> {
    Vmm::new(
        VmConfig {
            memory_mib: 1,
            vcpu_count: 4,
        },
        BTreeMap::from([("disk".into(), Disk(state.clone(), fault))]),
        None::<NoNet>,
        memory,
        Serial(state),
    )
}

fn run(vm: Vmm<Disk, NoNet, Serial>, state: Arc<State>, fault: Fault) -> Result<()> {
    let platform = Machine {
        state,
        fault,
        control: vm.control(),
        _local: Rc::new(()),
    };
    vm.run_with_platform(platform)
}

fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(
            std::time::Instant::now() < deadline,
            "control test timed out"
        );
        std::thread::yield_now();
    }
}

#[test]
fn pause_resume_memory_and_stop_are_synchronous() {
    for fault in [Fault::None, Fault::Pause, Fault::Resume, Fault::Flush] {
        let state = Arc::new(State::default());
        let vm = new_vm(state.clone(), fault, Some(VirtioMem::new(128, 2).unwrap()));
        let c = vm.control();
        let final_control = c.clone();
        let other = state.clone();
        let mem = c.clone();
        let t = std::thread::spawn(move || {
            wait(|| other.output.load(Ordering::SeqCst) > 10);
            let paused = c.pause();
            if fault == Fault::Pause {
                assert!(paused.is_err());
            } else {
                paused.unwrap();
                c.pause().unwrap();
                assert_eq!(c.status().lifecycle, VmLifecycle::Paused);
                let ticks = other.ticks.load(Ordering::SeqCst);
                let output = other.output.load(Ordering::SeqCst);
                let reads = other.reads.load(Ordering::SeqCst);
                mem.set_requested_mib(6).unwrap();
                assert!(mem.set_requested_mib(3).is_err());
                assert_eq!(mem.memory_status().unwrap().requested_size_mib, 6);
                std::thread::sleep(Duration::from_millis(50));
                assert_eq!(other.ticks.load(Ordering::SeqCst), ticks);
                assert_eq!(other.output.load(Ordering::SeqCst), output);
                assert_eq!(other.reads.load(Ordering::SeqCst), reads);
                assert_eq!(mem.memory_status().unwrap().plugged_size_mib, 0);
                let resumed = c.resume();
                if fault == Fault::Resume {
                    assert!(resumed.is_err());
                } else {
                    resumed.unwrap();
                    c.resume().unwrap();
                    wait(|| other.output.load(Ordering::SeqCst) > output);
                    wait(|| other.reads.load(Ordering::SeqCst) > reads);
                    c.pause().unwrap();
                }
            }
            let stops: Vec<_> = (0..4)
                .map(|_| {
                    let c = c.clone();
                    std::thread::spawn(move || c.stop())
                })
                .collect();
            for t in stops {
                assert_eq!(t.join().unwrap().is_err(), fault == Fault::Flush);
            }
            assert_eq!(c.status().lifecycle, VmLifecycle::Stopped);
            assert_eq!(
                mem.memory_status().unwrap().lifecycle,
                MemoryLifecycle::Stopped
            );
            let log = other.log.lock().unwrap();
            let pos = |s| log.iter().position(|v| *v == s).unwrap();
            assert!(pos("stop") < pos("flush"));
            assert!(pos("flush") < pos("unmap"));
            assert!(pos("unmap") < pos("destroy"));
            assert!(pos("destroy") < pos("disk-drop"));
        });
        assert_eq!(run(vm, state, fault).is_err(), fault != Fault::None);
        t.join().unwrap();
        assert_eq!(
            final_control.status().final_error.is_some(),
            fault != Fault::None
        );
    }
}

#[test]
fn startup_failure_and_cancelled_run() {
    for block in [1, 2, 4, 256] {
        for cancel in [false, true] {
            let state = Arc::new(State::default());
            let vm = new_vm(
                state.clone(),
                Fault::Prepare,
                Some(VirtioMem::new(128.max(block), block).unwrap()),
            );
            let control = vm.control();
            if cancel {
                control.stop().unwrap();
            }
            assert!(run(vm, state.clone(), Fault::Prepare).is_err());
            assert_eq!(control.status().lifecycle, VmLifecycle::Stopped);
            assert_eq!(control.status().final_error.is_some(), !cancel);
            control.stop().unwrap();
            assert_eq!(
                control.memory_status().unwrap().lifecycle,
                MemoryLifecycle::Stopped
            );
            assert!(control.set_requested_mib(0).is_err());
            assert!(state.log.lock().unwrap().contains(&"disk-drop"));
        }
    }
}

#[test]
fn stop_during_startup_waits_for_cleanup() {
    use std::sync::mpsc;

    struct Starting(Machine, mpsc::Sender<()>, mpsc::Receiver<()>);

    impl Platform for Starting {
        type Vm = Machine;

        fn layout(
            &self,
            c: &VmConfig,
            d: &DeviceRequirements,
        ) -> std::result::Result<MachineLayout, PlatformError> {
            self.0.layout(c, d)
        }

        fn create(&self, c: &VmConfig) -> std::result::Result<Machine, PlatformError> {
            self.1.send(()).unwrap();
            self.2.recv().unwrap();
            self.0.create(c)
        }
    }
    let state = Arc::new(State::default());
    let vm = new_vm(state.clone(), Fault::None, None);
    let c = vm.control();
    let (entered, entering) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let other = c.clone();
    let observed = state.clone();
    let t = std::thread::spawn(move || {
        entering.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(other.status().lifecycle, VmLifecycle::Starting);
        assert!(other.pause().is_err());
        assert!(other.resume().is_err());
        let stop = other.clone();
        let t = std::thread::spawn(move || stop.stop());
        wait(|| other.status().lifecycle == VmLifecycle::Stopping);
        assert!(!t.is_finished());
        release.send(()).unwrap();
        t.join().unwrap().unwrap();
        assert!(observed.log.lock().unwrap().contains(&"disk-drop"));
        assert_eq!(observed.ticks.load(Ordering::SeqCst), 0);
    });
    let platform = Starting(
        Machine {
            state: state.clone(),
            fault: Fault::None,
            control: c,
            _local: Rc::new(()),
        },
        entered,
        gate,
    );
    vm.run_with_platform(platform).unwrap();
    t.join().unwrap();
}

#[test]
fn memory_capability_targets_and_device_ownership() {
    for configured in [false, true] {
        for cancel in [false, true] {
            let state = Arc::new(State::default());
            let vm = new_vm(
                state.clone(),
                Fault::None,
                configured.then(|| VirtioMem::new(256, 2).unwrap()),
            );
            let c = vm.control();
            let other = c.clone();
            assert_eq!(c.supports_memory(), configured);
            if configured {
                assert_eq!(
                    c.memory_status().unwrap().lifecycle,
                    MemoryLifecycle::Created
                );
                assert!(!c.memory_status().unwrap().driver_ready);
                other.set_requested_mib(6).unwrap();
                assert_eq!(c.memory_status().unwrap().requested_size_mib, 6);
                for invalid in [1, 384, u64::MAX] {
                    assert!(c.set_requested_mib(invalid).is_err());
                }
                assert_eq!(c.memory_status().unwrap().requested_size_mib, 6);
            } else {
                assert_eq!(
                    c.memory_status().unwrap_err().to_string(),
                    "virtio-mem is not configured"
                );
                assert_eq!(
                    c.set_requested_mib(0).unwrap_err().to_string(),
                    "virtio-mem is not configured"
                );
            }
            if cancel {
                c.stop().unwrap();
                assert!(!state.log.lock().unwrap().contains(&"disk-drop"));
                if configured {
                    assert_eq!(
                        c.memory_status().unwrap().lifecycle,
                        MemoryLifecycle::Stopped
                    );
                }
                assert!(c.set_requested_mib(0).is_err());
            }
            drop(vm);
            assert!(state.log.lock().unwrap().contains(&"disk-drop"));
            assert_eq!(c.status().lifecycle, VmLifecycle::Stopped);
            assert_eq!(c.supports_memory(), configured);
            assert!(other.set_requested_mib(0).is_err());
            if configured {
                let status = other.memory_status().unwrap();
                assert_eq!(status.lifecycle, MemoryLifecycle::Stopped);
                assert_eq!(status.requested_size_mib, 6);
            }
        }
    }
}
