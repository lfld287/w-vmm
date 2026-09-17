use anyhow::{Context, Result};
use signal_hook::{
    SigId,
    consts::{SIGHUP, SIGINT, SIGTERM},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub struct Terminal {
    saved: Option<libc::termios>,
    flags: i32,
    signals: Vec<SigId>,
    pub stop: Arc<AtomicBool>,
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

    pub fn input(&self, capacity: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0; capacity.min(256)];
        if buf.is_empty() {
            return Ok(buf);
        }
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) {
                return Ok(vec![]);
            }
            return Err(e.into());
        }
        buf.truncate(n as usize);
        if let Some(p) = buf.iter().position(|b| *b == 0x1d) {
            self.stop.store(true, Ordering::Relaxed);
            buf.truncate(p);
        }
        Ok(buf)
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
