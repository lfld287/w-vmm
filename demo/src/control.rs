//! Optional local, bounded newline-delimited JSON control server.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::os::fd::AsRawFd;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};
use w_vmm::MemoryControl;
const LIMIT: usize = 4096;
const TIMEOUT: Duration = Duration::from_millis(500);
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    MemoryStatus,
    MemorySet { requested_mib: u64 },
}
fn status(control: &MemoryControl) -> Value {
    let s = control.status();
    json!({"region_size_mib": s.region_size_mib, "requested_size_mib": s.requested_size_mib, "plugged_size_mib": s.plugged_size_mib, "driver_ready": s.driver_ready, "lifecycle": format!("{:?}", s.lifecycle)})
}
fn line(stream: &mut UnixStream) -> Result<Vec<u8>> {
    // Absolute deadline also bounds clients that drip one byte before each timeout.
    let end = std::time::Instant::now() + TIMEOUT;
    let mut data = Vec::new();
    loop {
        let remaining = end.saturating_duration_since(std::time::Instant::now());
        ensure!(!remaining.is_zero(), "control request timeout");
        let mut fd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, remaining.as_millis().max(1) as i32) };
        if ready < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        ensure!(ready > 0, "control request timeout");
        let mut byte = [0];
        ensure!(stream.read(&mut byte)? == 1, "disconnected before newline");
        if byte[0] == b'\n' {
            return Ok(data);
        }
        ensure!(data.len() < LIMIT, "control message too long");
        data.push(byte[0]);
    }
}
fn serve(mut stream: UnixStream, control: &MemoryControl) -> Result<()> {
    stream
        .set_write_timeout(Some(TIMEOUT))
        .context("set write timeout")?;
    let result = (|| -> Result<Value> {
        match serde_json::from_slice::<Request>(&line(&mut stream)?)? {
            Request::MemoryStatus => {}
            Request::MemorySet { requested_mib } => control.set_requested_mib(requested_mib)?,
        }
        Ok(json!({"ok": true, "status": status(control)}))
    })();
    let response = result.unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()}));
    writeln!(stream, "{response}")?;
    Ok(())
}
pub struct Server {
    path: PathBuf,
    identity: (u64, u64),
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    pub fn bind(path: &Path, control: MemoryControl) -> Result<Self> {
        // bind atomically refuses any existing file, symlink or socket.
        let listener = UnixListener::bind(path)?;
        let meta = fs::symlink_metadata(path)?;
        let mut server = Self {
            path: path.to_owned(),
            identity: (meta.dev(), meta.ino()),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let stop = server.stop.clone();
        server.thread = Some(
            std::thread::Builder::new()
                .name("memory-control".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let _ = serve(stream, &control);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(10))
                            }
                            Err(_) => break,
                        }
                    }
                })?,
        );
        Ok(server)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.identity) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
pub fn request(path: &Path, request: Request) -> Result<Value> {
    let mut stream = UnixStream::connect(path).context("connect control socket")?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .context("set write timeout")?;
    writeln!(stream, "{}", serde_json::to_string(&request)?)?;
    Ok(serde_json::from_slice(&line(&mut stream)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use w_vmm::{VirtioMemConfig, VmConfig, Vmm};
    #[test]
    fn socket_control_conflict_disconnect_cleanup() {
        let dir = std::env::temp_dir().join(format!("w-vmm-control-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        let vm = Vmm::new(VmConfig {
            virtio_mem: Some(VirtioMemConfig {
                region_size_mib: 128,
                requested_size_mib: 0,
            }),
            ..VmConfig::default()
        });
        let control = vm.memory_control().unwrap();
        let server = Server::bind(&path, control.clone()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(Server::bind(&path, control).is_err());
        assert_eq!(
            request(&path, Request::MemorySet { requested_mib: 64 }).unwrap()["status"]["requested_size_mib"],
            64
        );
        assert_eq!(
            request(&path, Request::MemorySet { requested_mib: 3 }).unwrap()["ok"],
            false
        );
        drop(UnixStream::connect(&path).unwrap());
        let mut long = UnixStream::connect(&path).unwrap();
        long.write_all(&vec![b'x'; LIMIT + 1]).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&line(&mut long).unwrap()).unwrap()["ok"],
            false
        );
        drop(vm);
        assert_eq!(
            request(&path, Request::MemorySet { requested_mib: 0 }).unwrap()["ok"],
            false
        );
        assert_eq!(
            request(&path, Request::MemoryStatus).unwrap()["status"]["lifecycle"],
            "Stopped"
        );
        fs::remove_file(&path).unwrap();
        fs::write(&path, "replacement").unwrap();
        drop(server);
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
        fs::remove_dir_all(dir).unwrap();
    }
}
