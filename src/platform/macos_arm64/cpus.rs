//! vCPU ownership, PSCI power state and cancellable quiescence.
use super::hvf;
use anyhow::{Result, bail, ensure};
use std::{
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::JoinHandle,
    time::Duration,
};

pub struct Access {
    pub addr: u64,
    pub width: usize,
    pub write: bool,
    pub value: u64,
    pub reply: mpsc::SyncSender<u64>,
}

pub enum Event {
    Access(Access),
    Failed(anyhow::Error),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Power {
    Off,
    Pending(u64, u64),
    On,
}

struct State {
    stop: bool,
    paused: bool,
    started: bool,
    power: Vec<Power>,
    running: Vec<bool>,
    ids: Vec<Option<u64>>,
}

pub struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn new(count: u32) -> Self {
        Self {
            state: Mutex::new(State {
                stop: false,
                paused: false,
                started: false,
                power: vec![Power::Off; count as usize],
                running: vec![false; count as usize],
                ids: vec![None; count as usize],
            }),
            changed: Condvar::new(),
        }
    }

    fn kick(state: &State) {
        for id in state.ids.iter().flatten() {
            hvf::kick(*id);
        }
    }

    pub fn stop(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.stop = true;
        Self::kick(&s);
        self.changed.notify_all();
    }

    fn on(&self, target: u64, entry: u64, context: u64) -> i64 {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(power) = usize::try_from(target)
            .ok()
            .and_then(|i| s.power.get_mut(i))
        else {
            return -2;
        };
        if entry & 3 != 0 {
            return -9;
        }
        match power {
            Power::On => -4,
            Power::Pending(..) => -5,
            Power::Off => {
                *power = Power::Pending(entry, context);
                self.changed.notify_all();
                0
            }
        }
    }

    fn affinity(&self, target: u64, level: u64) -> i64 {
        if level > 3 {
            return -2;
        }
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if level == 0 {
            match usize::try_from(target).ok().and_then(|i| s.power.get(i)) {
                Some(Power::On) => 0,
                Some(Power::Off) => 1,
                Some(Power::Pending(..)) => 2,
                None => -2,
            }
        } else if target & !((1u64 << (level * 8)) - 1) == 0 {
            if s.power.iter().any(|p| *p != Power::Off) {
                0
            } else {
                1
            }
        } else {
            -2
        }
    }

    fn wait_io(&self, index: usize) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.running[index] = false;
        self.changed.notify_all();
    }

    fn complete_io(&self, index: usize) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while s.paused && !s.stop {
            s = self.changed.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if s.stop {
            return false;
        }
        s.running[index] = true;
        true
    }

    pub fn pause(&self) -> Result<()> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.paused = true;
        while s.running.iter().any(|r| *r) && !s.stop {
            Self::kick(&s);
            s = self
                .changed
                .wait_timeout(s, Duration::from_millis(2))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        ensure!(!s.stop, "VM stopped during memory transaction");
        Ok(())
    }
}

impl Shared {
    pub fn resume(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.paused = false;
        self.changed.notify_all();
    }
}

pub struct Cpus {
    pub shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Cpus {
    pub fn create(count: u32, events: mpsc::Sender<Event>) -> Result<Self> {
        let mut cpus = Self {
            shared: Arc::new(Shared::new(count)),
            threads: Vec::new(),
        };
        // Sequential creation fixes HVF GIC redistributor order. Nothing runs yet.
        for index in 0..count as usize {
            let shared = cpus.shared.clone();
            let events = events.clone();
            let (tx, rx) = mpsc::sync_channel(1);
            cpus.threads.push(
                std::thread::Builder::new()
                    .name(format!("vcpu-{index}"))
                    .spawn(move || {
                        let cpu = match hvf::Vcpu::new(index as u64) {
                            Ok(cpu) => cpu,
                            Err(e) => {
                                let _ = tx.send(Err(e));
                                return;
                            }
                        };
                        shared.state.lock().unwrap_or_else(|e| e.into_inner()).ids[index] =
                            Some(cpu.id);
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                            || -> Result<()> {
                                tx.send(Ok(()))
                                    .map_err(|_| anyhow::anyhow!("startup cancelled"))?;
                                worker(&cpu, index, &shared, &events)
                            },
                        ));
                        let error = match result {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e),
                            Err(_) => Some(anyhow::anyhow!("vCPU thread panicked")),
                        };
                        if let Some(error) = error {
                            let _ = events.send(Event::Failed(error));
                        }
                        // Never destroy a GIC CPU resource while another vCPU executes.
                        // This also covers errors, panic, and a cancelled partial startup.
                        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                        state.running[index] = false;
                        state.stop = true;
                        shared.changed.notify_all();
                        while state.running.iter().any(|r| *r) {
                            Shared::kick(&state);
                            state = shared
                                .changed
                                .wait_timeout(state, Duration::from_millis(2))
                                .unwrap_or_else(|e| e.into_inner())
                                .0;
                        }
                        state.ids[index] = None;
                        drop(state);
                        drop(cpu);
                    })?,
            );
            rx.recv()
                .map_err(|_| anyhow::anyhow!("vCPU initialization thread failed"))??;
        }
        Ok(cpus)
    }

    pub fn start(&self, entry: u64, dtb: u64) {
        let mut s = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.power[0] = Power::Pending(entry, dtb);
        s.started = true;
        self.shared.changed.notify_all();
    }

    pub fn stopping(&self) -> bool {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stop
    }
}

impl Drop for Cpus {
    fn drop(&mut self) {
        self.shared.stop();
        while self.threads.iter().any(|t| !t.is_finished()) {
            self.shared.stop();
            std::thread::sleep(Duration::from_millis(2));
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn worker(
    cpu: &hvf::Vcpu,
    index: usize,
    shared: &Shared,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let mut s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        s.running[index] = false;
        shared.changed.notify_all();
        while !s.stop && (!s.started || s.paused || s.power[index] == Power::Off) {
            s = shared.changed.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if s.stop {
            return Ok(());
        }
        if let Power::Pending(entry, context) = s.power[index] {
            cpu.boot(entry, context)?;
            s.power[index] = Power::On;
        }
        s.running[index] = true;
        drop(s);
        let exit = cpu.run();
        let s = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.stop {
            return Ok(());
        }
        drop(s);
        let exit = exit?;
        match exit.reason {
            0 => continue,
            1 => {
                let e = exit.exception;
                let esr = e.syndrome;
                match esr >> 26 {
                    0x16 => {
                        let function = cpu.get(0)?;
                        let wide = function & (1 << 30) != 0;
                        let arg = |i| cpu.get(i).map(|v| if wide { v } else { v & 0xffff_ffff });
                        let value = match function {
                            0x84000000 => 2,
                            0x84000002 => {
                                shared.state.lock().unwrap_or_else(|e| e.into_inner()).power
                                    [index] = Power::Off;
                                continue;
                            }
                            0x84000003 | 0xc4000003 => shared.on(arg(1)?, arg(2)?, arg(3)?),
                            0x84000004 | 0xc4000004 => shared.affinity(arg(1)?, arg(2)?),
                            0x84000006 => 2, // MIGRATE_INFO_TYPE: no trusted OS.
                            0x84000008 | 0x84000009 => {
                                events.send(Event::Shutdown)?;
                                shared.stop();
                                return Ok(());
                            }
                            _ => -1,
                        };
                        cpu.set(0, value as u64)?;
                    }
                    0x24 => {
                        ensure!(
                            esr & (1 << 24) != 0 && esr & ((1 << 7) | (1 << 8)) == 0,
                            "unsupported data abort: {e:?}"
                        );
                        let width = 1usize << ((esr >> 22) & 3);
                        let reg = ((esr >> 16) & 31) as u32;
                        let write = esr & (1 << 6) != 0;
                        let (reply, rx) = mpsc::sync_channel(1);
                        events.send(Event::Access(Access {
                            addr: e.physical_address,
                            width,
                            write,
                            value: if write && reg != 31 { cpu.get(reg)? } else { 0 },
                            reply,
                        }))?;
                        // A queued access owns its reply channel; it remains pending
                        // across pause. No guest register work occurs while waiting.
                        shared.wait_io(index);
                        let value = loop {
                            match rx.recv_timeout(Duration::from_millis(10)) {
                                Ok(value) => break value,
                                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                                Err(_)
                                    if shared
                                        .state
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .stop =>
                                {
                                    return Ok(());
                                }
                                Err(_) => {}
                            }
                        };
                        if !shared.complete_io(index) {
                            return Ok(());
                        }
                        if !write && reg != 31 {
                            let mut value = value;
                            if esr & (1 << 21) != 0 {
                                value =
                                    ((value << (64 - width * 8)) as i64 >> (64 - width * 8)) as u64;
                            }
                            if esr & (1 << 15) == 0 {
                                value &= 0xffff_ffff;
                            }
                            cpu.set(reg, value)?;
                        }
                        cpu.advance()?;
                    }
                    0x18 => {
                        let sysreg = (((esr >> 20) & 3) << 14)
                            | (((esr >> 14) & 7) << 11)
                            | (((esr >> 10) & 15) << 7)
                            | (((esr >> 1) & 15) << 3)
                            | ((esr >> 17) & 7);
                        ensure!(
                            matches!(sysreg, 0x8084 | 0x808c | 0x809c),
                            "unsupported sysreg {sysreg:#x}, ESR={esr:#x}"
                        );
                        let rt = ((esr >> 5) & 31) as u32;
                        if esr & 1 != 0 && rt != 31 {
                            cpu.set(rt, 0)?;
                        }
                        cpu.advance()?;
                    }
                    1 => cpu.advance()?,
                    _ => bail!("unhandled HVF exception {e:?}, PC={:#x}", cpu.get(31)?),
                }
            }
            _ => bail!("unexpected HVF exit {exit:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psci_states_and_affinity() {
        let s = Shared::new(4);
        assert_eq!(s.affinity(3, 0), 1);
        assert_eq!(s.on(3, 0x8000, 42), 0);
        assert_eq!(s.on(3, 0x8000, 0), -5);
        assert_eq!(s.affinity(3, 0), 2);
        s.state.lock().unwrap_or_else(|e| e.into_inner()).power[3] = Power::On;
        assert_eq!(s.on(3, 0x8000, 0), -4);
        assert_eq!(s.affinity(3, 0), 0);
        s.state.lock().unwrap_or_else(|e| e.into_inner()).power[3] = Power::Off;
        assert_eq!(s.on(3, 0x9000, 9), 0);
        assert_eq!(s.on(4, 0, 0), -2);
        assert_eq!(s.on(u64::MAX, 0, 0), -2);
        assert_eq!(s.on(0, 3, 0), -9);
        assert_eq!(s.affinity(4, 0), -2);
        assert_eq!(s.affinity(0, 4), -2);
        assert_eq!(s.affinity(0, 1), 0);
        assert_eq!(s.affinity(3, 1), 0);
        assert_eq!(s.affinity(0x100, 1), -2);
    }

    #[test]
    fn pause_includes_offline_and_mmio_waiters_and_stop_cancels() {
        let s = Arc::new(Shared::new(4));
        s.state.lock().unwrap_or_else(|e| e.into_inner()).running[0] = true;
        let worker = s.clone();
        let t = std::thread::spawn(move || {
            let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
            while !state.paused {
                drop(state);
                std::thread::yield_now();
                state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
            }
            state.running[0] = false;
            worker.changed.notify_all();
        });
        {
            s.pause().unwrap();
            assert!(s.state.lock().unwrap_or_else(|e| e.into_inner()).paused);
        }
        s.resume();
        t.join().unwrap();
        assert!(!s.state.lock().unwrap_or_else(|e| e.into_inner()).paused);
        s.stop();
        assert!(s.pause().is_err());
    }

    #[test]
    fn mmio_reply_cannot_cross_pause_barrier() {
        for stop in [false, true] {
            let s = Arc::new(Shared::new(4));
            s.state.lock().unwrap().running[0] = true;
            s.wait_io(0);
            s.pause().unwrap();
            let worker = s.clone();
            let (tx, rx) = mpsc::channel();
            let t = std::thread::spawn(move || {
                tx.send(worker.complete_io(0)).unwrap();
            });
            assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
            assert!(!s.state.lock().unwrap().running[0]);
            if stop {
                s.stop();
            } else {
                s.resume();
            }
            assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), !stop);
            t.join().unwrap();
        }
    }

    #[test]
    fn partial_startup_is_cancellable() {
        let s = Arc::new(Shared::new(4));
        let worker = s.clone();
        let t = std::thread::spawn(move || {
            let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
            while !state.stop {
                state = worker.changed.wait(state).unwrap();
            }
        });
        s.stop();
        t.join().unwrap();
    }
}
