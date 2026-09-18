use anyhow::{Context, Result};
use signal_hook::{
    SigId,
    consts::{SIGHUP, SIGINT, SIGTERM},
};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use w_vmm::{VmControl, serial::SerialIo};

pub struct Terminal {
    saved: Option<libc::termios>,
    flags: i32,
    notify: UnixStream,
}

/// Owns signal registrations and the blocking stop thread.
/// Keep this guard until `Vmm::run` returns; never drop it in a VMM callback.
pub struct TerminalGuard {
    signals: Vec<SigId>,
    notify: UnixStream,
    closing: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TerminalGuard {
    fn new(control: VmControl) -> Result<Self> {
        let (mut read, notify) = UnixStream::pair()?;
        notify.set_nonblocking(true)?;
        let mut guard = Self {
            signals: vec![],
            notify,
            closing: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        for sig in [SIGINT, SIGTERM, SIGHUP] {
            guard.signals.push(signal_hook::low_level::pipe::register(
                sig,
                guard.notify.try_clone()?,
            )?);
        }
        let closing = guard.closing.clone();
        guard.thread = Some(thread::Builder::new().name("terminal-stop".into()).spawn(
            move || {
                let mut byte = [0];
                loop {
                    match read.read(&mut byte) {
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Ok(0) | Err(_) => return,
                        Ok(_) => break,
                    }
                }
                if !closing.load(Ordering::Acquire) {
                    // Stop waits for serial destruction and all VM cleanup. Only
                    // the caller's guard may join this thread after run returns.
                    let _ = control.stop();
                }
            },
        )?);
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        for id in self.signals.drain(..) {
            signal_hook::low_level::unregister(id);
        }
        self.closing.store(true, Ordering::Release);
        // EOF wakes the reader even if no signal or Ctrl-] ever arrived.
        let _ = self.notify.shutdown(Shutdown::Write);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Terminal {
    pub fn new(control: VmControl) -> Result<(Self, TerminalGuard)> {
        let guard = TerminalGuard::new(control)?;
        let flags = unsafe { libc::fcntl(0, libc::F_GETFL) };
        let mut t = Self {
            saved: None,
            flags,
            notify: guard.notify.try_clone()?,
        };
        unsafe {
            if libc::isatty(0) == 1 {
                let mut old = std::mem::zeroed();
                if libc::tcgetattr(0, &mut old) != 0 {
                    return Err(io::Error::last_os_error()).context("get terminal mode");
                }
                t.saved = Some(old);
                let mut raw = old;
                libc::cfmakeraw(&mut raw);
                if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                    return Err(io::Error::last_os_error()).context("set terminal raw mode");
                }
            }
            if flags >= 0 && libc::fcntl(0, libc::F_SETFL, flags | libc::O_NONBLOCK) != 0 {
                return Err(io::Error::last_os_error()).context("set stdin nonblocking");
            }
        }
        Ok((t, guard))
    }
}

impl Write for Terminal {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        io::stdout().write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

impl SerialIo for Terminal {
    fn recv(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let n = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        if let Some(p) = buffer[..n].iter().position(|b| *b == 0x1d) {
            match self.notify.write_all(&[1]) {
                Ok(_) => (),
                // A full channel already contains a stop notification; a
                // closed reader has already handled one.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    ()
                }
                Err(e) => return Err(e),
            }
            return Ok(p);
        }
        Ok(n)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            if let Some(old) = self.saved {
                libc::tcsetattr(0, libc::TCSANOW, &old);
            }
            if self.flags >= 0 {
                libc::fcntl(0, libc::F_SETFL, self.flags);
            }
        }
    }
}
