//! Synchronous, thread-safe control of a VM owned by another thread.
use crate::memory::{MemoryControl, MemoryLifecycle, MemoryStatus};
use anyhow::{Result, bail, ensure};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread::ThreadId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmLifecycle {
    Created,
    Starting,
    Running,
    Paused,
    Stopping,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmStatus {
    pub lifecycle: VmLifecycle,
    /// Final startup, runtime or cleanup error, retained after termination.
    pub final_error: Option<String>,
}

/// Cloneable, Send + Sync handle; platform operations stay on the run thread.
/// Blocking methods have no timeout and must not be called by that thread
/// (including device callbacks). `status` may be read from any thread.
#[derive(Clone)]
pub struct VmControl(Arc<Shared>);

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    memory: Option<MemoryControl>,
}

struct State {
    status: VmStatus,
    owner: Option<ThreadId>,
    queue: VecDeque<Request>,
    cleanup_error: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) enum Operation {
    Pause,
    Resume,
}

pub(crate) struct Request {
    pub operation: Operation,
    reply: mpsc::Sender<std::result::Result<(), String>>,
}

fn result(error: &Option<String>) -> Result<()> {
    match error {
        Some(e) => bail!("{e}"),
        None => Ok(()),
    }
}

impl VmControl {
    pub(crate) fn new(memory: Option<MemoryControl>) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(State {
                status: VmStatus {
                    lifecycle: VmLifecycle::Created,
                    final_error: None,
                },
                owner: None,
                queue: VecDeque::new(),
                cleanup_error: None,
            }),
            changed: Condvar::new(),
            memory,
        }))
    }

    /// Whether virtio-mem was configured, independent of driver readiness.
    pub fn supports_memory(&self) -> bool {
        self.0.memory.is_some()
    }

    fn memory(&self) -> Result<&MemoryControl> {
        self.0
            .memory
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("virtio-mem is not configured"))
    }

    /// Read the current or final memory state; errors if virtio-mem is absent.
    pub fn memory_status(&self) -> Result<MemoryStatus> {
        Ok(self.memory()?.status())
    }

    /// Accept a target before startup, while running, or while paused.
    /// The guest reaches it asynchronously while running. Stopped VMs reject it.
    pub fn set_requested_mib(&self, requested: u64) -> Result<()> {
        self.memory()?.set_requested_mib(requested)
    }

    fn stop_memory(&self) {
        if let Some(memory) = &self.0.memory {
            let mut state = memory.lock();
            state.lifecycle = MemoryLifecycle::Stopped;
            state.driver_ready = false;
        }
    }

    pub fn status(&self) -> VmStatus {
        self.0.state.lock().unwrap().status.clone()
    }

    /// Wait until all vCPUs and device processing are paused.
    pub fn pause(&self) -> Result<()> {
        self.submit(Operation::Pause)
    }

    /// Wait until running scheduling has been restored.
    pub fn resume(&self) -> Result<()> {
        self.submit(Operation::Resume)
    }

    fn submit(&self, operation: Operation) -> Result<()> {
        let (reply, rx) = mpsc::channel();
        {
            let mut s = self.0.state.lock().unwrap();
            ensure!(
                s.owner != Some(std::thread::current().id()),
                "blocking VM control on the VMM thread"
            );
            ensure!(
                matches!(
                    s.status.lifecycle,
                    VmLifecycle::Running | VmLifecycle::Paused
                ),
                "VM is not running"
            );
            s.queue.push_back(Request { operation, reply });
            self.0.changed.notify_all();
        }
        rx.recv()
            .map_err(|_| anyhow::anyhow!("VM control disconnected"))?
            .map_err(anyhow::Error::msg)
    }

    /// Wait for workers, disk flushing and resource destruction. Repeated calls
    /// return the saved cleanup result. Calling before run cancels this VM.
    /// In that case memory stops immediately; devices remain owned by `Vmm`
    /// until it is consumed or dropped.
    pub fn stop(&self) -> Result<()> {
        let mut s = self.0.state.lock().unwrap();
        ensure!(
            s.owner != Some(std::thread::current().id()),
            "blocking VM control on the VMM thread"
        );
        if s.status.lifecycle == VmLifecycle::Created {
            self.stop_memory();
            s.status.lifecycle = VmLifecycle::Stopped;
        } else if s.status.lifecycle != VmLifecycle::Stopped {
            s.status.lifecycle = VmLifecycle::Stopping;
        }
        Self::cancel(&mut s);
        self.0.changed.notify_all();
        while s.status.lifecycle != VmLifecycle::Stopped {
            s = self.0.changed.wait(s).unwrap();
        }
        result(&s.cleanup_error)
    }

    fn cancel(s: &mut State) {
        for r in s.queue.drain(..) {
            let _ = r.reply.send(Err("VM is stopping".into()));
        }
    }

    pub(crate) fn begin(&self) -> Result<()> {
        let mut s = self.0.state.lock().unwrap();
        ensure!(
            s.status.lifecycle == VmLifecycle::Created,
            "VM was cancelled before run"
        );
        s.owner = Some(std::thread::current().id());
        s.status.lifecycle = VmLifecycle::Starting;
        Ok(())
    }

    pub(crate) fn stopping(&self) -> bool {
        self.status().lifecycle == VmLifecycle::Stopping
    }

    pub(crate) fn running(&self) {
        let mut s = self.0.state.lock().unwrap();
        if s.status.lifecycle == VmLifecycle::Starting {
            s.status.lifecycle = VmLifecycle::Running;
        }
    }

    pub(crate) fn next(&self) -> Option<Request> {
        self.0.state.lock().unwrap().queue.pop_front()
    }

    pub(crate) fn complete(&self, request: Request, outcome: &Result<()>) {
        let mut s = self.0.state.lock().unwrap();
        if outcome.is_ok() && s.status.lifecycle != VmLifecycle::Stopping {
            s.status.lifecycle = match request.operation {
                Operation::Pause => VmLifecycle::Paused,
                Operation::Resume => VmLifecycle::Running,
            };
        }
        let _ = request
            .reply
            .send(outcome.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")));
    }

    pub(crate) fn wait(&self) {
        let mut s = self.0.state.lock().unwrap();
        while s.status.lifecycle == VmLifecycle::Paused && s.queue.is_empty() {
            s = self.0.changed.wait(s).unwrap();
        }
    }

    pub(crate) fn cleaning(&self) {
        let mut s = self.0.state.lock().unwrap();
        s.status.lifecycle = VmLifecycle::Stopping;
        Self::cancel(&mut s);
    }

    pub(crate) fn cleanup_result(&self, outcome: &Result<()>) {
        self.0.state.lock().unwrap().cleanup_error =
            outcome.as_ref().err().map(|e| format!("{e:#}"));
    }

    pub(crate) fn finish(&self, outcome: &Result<()>) {
        self.stop_memory();
        let mut s = self.0.state.lock().unwrap();
        s.status.lifecycle = VmLifecycle::Stopped;
        s.status.final_error = outcome.as_ref().err().map(|e| format!("{e:#}"));
        s.owner = None;
        Self::cancel(&mut s);
        self.0.changed.notify_all();
    }

    pub(crate) fn dropped(&self) {
        if self.status().lifecycle != VmLifecycle::Stopped {
            if std::thread::panicking() {
                let error = Err(anyhow::anyhow!("VMM thread panicked"));
                self.cleanup_result(&error);
                self.finish(&error);
            } else {
                self.finish(&Ok(()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unused_cancelled_and_owner_thread() {
        fn thread_safe<T: Clone + Send + Sync>() {}
        thread_safe::<VmControl>();
        let c = VmControl::new(None);
        assert!(c.pause().is_err());
        assert!(c.resume().is_err());
        c.stop().unwrap();
        c.stop().unwrap();
        assert!(c.begin().is_err());
        c.dropped();
        assert_eq!(c.status().lifecycle, VmLifecycle::Stopped);
        let c = VmControl::new(None);
        c.dropped();
        assert_eq!(c.status().lifecycle, VmLifecycle::Stopped);
        let c = VmControl::new(None);
        c.begin().unwrap();
        c.running();
        for r in [c.pause(), c.resume(), c.stop()] {
            assert!(r.unwrap_err().to_string().contains("VMM thread"));
        }
    }

    #[test]
    fn paused_wait_ignores_spurious_notifications_and_keeps_queued_wakes() {
        use std::time::Duration;
        let c = VmControl::new(None);
        c.0.state.lock().unwrap().status.lifecycle = VmLifecycle::Paused;
        let (done, rx) = mpsc::channel();
        let waiter = c.clone();
        let t = std::thread::spawn(move || {
            waiter.wait();
            done.send(()).unwrap();
        });
        c.0.changed.notify_all();
        // In particular, the former 20 ms timed wait must not return here.
        assert!(rx.recv_timeout(Duration::from_millis(60)).is_err());
        let (reply, _) = mpsc::channel();
        {
            let mut s = c.0.state.lock().unwrap();
            s.queue.push_back(Request {
                operation: Operation::Resume,
                reply,
            });
            c.0.changed.notify_all();
        }
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        t.join().unwrap();
        // A notification that arrives before wait cannot be lost: the queued
        // request itself prevents sleeping, even with lifecycle still Paused.
        c.wait();
        c.next().unwrap();
        c.0.state.lock().unwrap().status.lifecycle = VmLifecycle::Stopping;
        c.wait();
    }

    #[test]
    fn stop_cancels_queue_and_waits_for_saved_cleanup_result() {
        let c = VmControl::new(None);
        c.begin().unwrap();
        c.running();
        let other = c.clone();
        let pause = std::thread::spawn(move || other.pause());
        while c.0.state.lock().unwrap().queue.is_empty() {
            std::thread::yield_now();
        }
        let stops: Vec<_> = (0..4)
            .map(|_| {
                let c = c.clone();
                std::thread::spawn(move || c.stop())
            })
            .collect();
        while !c.stopping() {
            std::thread::yield_now();
        }
        assert!(pause.join().unwrap().is_err());
        assert!(stops.iter().all(|t| !t.is_finished()));
        assert!(c.next().is_none());
        let error = Err(anyhow::anyhow!("flush failed"));
        c.cleanup_result(&error);
        c.finish(&error);
        for t in stops {
            assert_eq!(t.join().unwrap().unwrap_err().to_string(), "flush failed");
        }
        assert_eq!(c.stop().unwrap_err().to_string(), "flush failed");
        assert_eq!(c.status().final_error.as_deref(), Some("flush failed"));
    }
}
