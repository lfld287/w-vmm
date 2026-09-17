use anyhow::{Context, Result};
use signal_hook::{
    SigId,
    consts::{SIGHUP, SIGINT, SIGTERM},
};
use std::io::{self, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use w_vmm::serial::SerialIo;

pub struct Terminal {
    saved: Option<libc::termios>,
    flags: i32,
    signals: Vec<SigId>,
    stop: Arc<AtomicBool>,
}

impl Terminal {
    pub fn new() -> Result<Self> {
        let flags = unsafe { libc::fcntl(0, libc::F_GETFL) };
        let mut t = Self {
            saved: None,
            flags,
            signals: vec![],
            stop: Arc::new(AtomicBool::new(false)),
        };
        for sig in [SIGINT, SIGTERM, SIGHUP] {
            t.signals
                .push(signal_hook::flag::register(sig, t.stop.clone())?);
        }
        unsafe {
            if libc::isatty(0) == 1 {
                let mut old = std::mem::zeroed();
                if libc::tcgetattr(0, &mut old) != 0 {
                    return Err(std::io::Error::last_os_error()).context("get terminal mode");
                }
                t.saved = Some(old);
                let mut raw = old;
                libc::cfmakeraw(&mut raw);
                if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                    return Err(std::io::Error::last_os_error()).context("set terminal raw mode");
                }
            }
            if flags >= 0 && libc::fcntl(0, libc::F_SETFL, flags | libc::O_NONBLOCK) != 0 {
                return Err(std::io::Error::last_os_error()).context("set stdin nonblocking");
            }
        }
        Ok(t)
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
            self.stop.store(true, Ordering::Relaxed);
            return Ok(p);
        }
        Ok(n)
    }

    fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
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
        for id in self.signals.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
