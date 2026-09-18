//! Errors for CLI arguments, terminal setup and the local control protocol.
use thiserror::Error;
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Arguments(#[from] ArgumentError),
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Vmm(#[from] w_vmm::Error),
    #[error(transparent)]
    Storage(#[from] w_vmm::error::StorageError),
    #[error(transparent)]
    Net(#[from] w_vmm::error::NetError),
    #[error(transparent)]
    Memory(#[from] w_vmm::error::MemoryError),
}
#[derive(Debug, Error)]
pub enum ArgumentError {
    #[error("empty path for disk {name}")]
    EmptyDiskPath { name: String },
    #[error("duplicate disk name: {name}")]
    DuplicateDisk { name: String },
    #[error("usage: net-peer run [disk.qcow2 ...]")]
    PeerUsage,
    #[error("invalid vCPU count: {0}")]
    CpuCount(#[from] std::num::ParseIntError),
}
#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("terminal guard is already bound")]
    AlreadyBound,
    #[error("terminal I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("{operation}: {source}")]
    Operation {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
}
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("control request timeout")]
    Timeout,
    #[error("disconnected before {expected}")]
    Disconnected { expected: &'static str },
    #[error("control {kind} too long (limit {limit})")]
    TooLong { kind: &'static str, limit: usize },
    #[error("control request failed: {response}")]
    RequestFailed { response: serde_json::Value },
    #[error("control socket I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("{operation} {path:?}: {source}")]
    Operation {
        operation: &'static str,
        path: Option<std::path::PathBuf>,
        #[source]
        source: std::io::Error,
    },
    #[error("control JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Control(#[from] w_vmm::error::ControlError),
}
