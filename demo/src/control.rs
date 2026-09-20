//! Optional local, bounded newline-delimited JSON control server.
use crate::error::ProtocolError;
type Result<T> = std::result::Result<T, ProtocolError>;
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
use w_vmm::VmControl;
const LIMIT: usize = 4096;
const TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    Pause,
    Resume,
    Stop,
    Status,
    MemoryStatus,
    MemorySet { requested_mib: u64 },
}

fn status(control: &VmControl) -> Result<Value> {
    let s = control.memory_status()?;
    Ok(
        json!({"region_size_mib": s.region_size_mib, "block_size_mib": s.block_size_mib, "requested_size_mib": s.requested_size_mib, "plugged_size_mib": s.plugged_size_mib, "driver_ready": s.driver_ready, "lifecycle": format!("{:?}", s.lifecycle)}),
    )
}

fn line(stream: &mut UnixStream) -> Result<Vec<u8>> {
    // Absolute deadline also bounds clients that drip one byte before each timeout.
    let end = std::time::Instant::now() + TIMEOUT;
    let mut data = Vec::new();
    loop {
        let remaining = end.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(ProtocolError::Timeout);
        }
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
        if ready <= 0 {
            return Err(ProtocolError::Timeout);
        }
        let mut byte = [0];
        if stream.read(&mut byte)? != 1 {
            return Err(ProtocolError::Disconnected {
                expected: "newline",
            });
        }
        if byte[0] == b'\n' {
            return Ok(data);
        }
        if data.len() >= LIMIT {
            return Err(ProtocolError::TooLong {
                kind: "message",
                limit: LIMIT,
            });
        }
        data.push(byte[0]);
    }
}

fn serve(mut stream: UnixStream, vm: &VmControl) -> Result<()> {
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(|source| ProtocolError::Operation {
            operation: "set write timeout",
            path: None,
            source,
        })?;
    let result = (|| -> Result<Value> {
        let request = serde_json::from_slice::<Request>(&line(&mut stream)?)?;
        match request {
            Request::MemoryStatus | Request::MemorySet { .. } => {
                let control = vm;
                if let Request::MemorySet { requested_mib } = request {
                    control.set_requested_mib(requested_mib)?;
                }
                Ok(json!({"ok": true, "status": status(control)?}))
            }
            _ => {
                match request {
                    Request::Pause => vm.pause()?,
                    Request::Resume => vm.resume()?,
                    Request::Stop => vm.stop()?,
                    Request::Status => (),
                    _ => unreachable!(),
                }
                let state = vm.status();
                Ok(
                    json!({"ok": true, "status": {"lifecycle": format!("{:?}", state.lifecycle), "final_error": state.final_error}}),
                )
            }
        }
    })();
    let response =
        result.unwrap_or_else(|e| json!({"ok": false, "error": w_vmm::error::diagnostic(&e)}));
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
    pub fn bind(path: &Path, control: VmControl) -> Result<Self> {
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
                .name("vm-control".into())
                .spawn(move || {
                    let mut connections: Vec<JoinHandle<()>> = Vec::new();
                    while !stop.load(Ordering::Acquire) {
                        let mut i = 0;
                        while i < connections.len() {
                            if connections[i].is_finished() {
                                let _ = connections.swap_remove(i).join();
                            } else {
                                i += 1;
                            }
                        }
                        if connections.len() == 16 {
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let control = control.clone();
                                match std::thread::Builder::new()
                                    .name("vm-control-client".into())
                                    .spawn(move || {
                                        let _ = serve(stream, &control);
                                    }) {
                                    Ok(t) => connections.push(t),
                                    Err(_) => break,
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(10))
                            }
                            Err(_) => break,
                        }
                    }
                    for t in connections {
                        let _ = t.join();
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
    let mut stream = UnixStream::connect(path).map_err(|source| ProtocolError::Operation {
        operation: "connect control socket",
        path: Some(path.to_owned()),
        source,
    })?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(|source| ProtocolError::Operation {
            operation: "set write timeout",
            path: None,
            source,
        })?;
    writeln!(stream, "{}", serde_json::to_string(&request)?)?;
    // Operations have no completion deadline. Keep a bounded response buffer.
    let mut data = Vec::new();
    loop {
        let mut byte = [0];
        if stream.read(&mut byte)? != 1 {
            return Err(ProtocolError::Disconnected {
                expected: "response",
            });
        }
        if byte[0] == b'\n' {
            break;
        }
        if data.len() >= LIMIT {
            return Err(ProtocolError::TooLong {
                kind: "response",
                limit: LIMIT,
            });
        }
        data.push(byte[0]);
    }
    Ok(serde_json::from_slice(&data)?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use w_vmm::VirtioMem;
    pub(crate) fn test_vm(
        memory: Option<VirtioMem>,
    ) -> w_vmm::Vmm<w_vmm::storage::Disk, w_vmm::net::macos::Vmnet, TestSerial> {
        w_vmm::Vmm::new(
            w_vmm::VmConfig::default(),
            Default::default(),
            None,
            memory,
            TestSerial,
        )
    }
    pub(crate) struct TestSerial;
    impl Write for TestSerial {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl w_vmm::serial::SerialIo for TestSerial {
        fn recv(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn vm_only_concurrent_clients_and_stop_response() {
        let dir = std::env::temp_dir().join(format!("w-vmm-lifecycle-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        let vm = test_vm(None);
        let server = Server::bind(&path, vm.control()).unwrap();
        // A client sending no newline must not block status or stop clients.
        let idle = UnixStream::connect(&path).unwrap();
        let clients: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || request(&path, Request::Status).unwrap())
            })
            .collect();
        for t in clients {
            assert_eq!(t.join().unwrap()["status"]["lifecycle"], "Created");
        }
        assert_eq!(request(&path, Request::Pause).unwrap()["ok"], false);
        assert_eq!(request(&path, Request::Resume).unwrap()["ok"], false);
        assert_eq!(request(&path, Request::MemoryStatus).unwrap()["ok"], false);
        assert_eq!(
            request(&path, Request::MemorySet { requested_mib: 0 }).unwrap()["ok"],
            false
        );
        assert_eq!(
            request(&path, Request::Stop).unwrap()["status"]["lifecycle"],
            "Stopped"
        );
        assert_eq!(request(&path, Request::Stop).unwrap()["ok"], true);
        drop(idle);
        drop(server);
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn socket_control_conflict_disconnect_cleanup() {
        let dir = std::env::temp_dir().join(format!("w-vmm-control-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        let memory = VirtioMem::new(128, 2).unwrap();
        let vm = test_vm(Some(memory));
        let server = Server::bind(&path, vm.control()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(Server::bind(&path, vm.control()).is_err());
        assert_eq!(
            request(&path, Request::MemorySet { requested_mib: 128 }).unwrap()["status"]["requested_size_mib"],
            128
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
