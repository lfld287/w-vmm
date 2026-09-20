use clap::{Parser, Subcommand};
use std::{collections::BTreeMap, path::Path};
use w_vmm::{
    VirtioMem, VmConfig, Vmm,
    net::{NetDevice, macos::Vmnet},
    storage::Disk,
};
use w_vmm_demo::{
    Result,
    error::{ArgumentError, ProtocolError},
};
use w_vmm_demo::{
    control::{self, Request, Server},
    terminal::Terminal,
};

#[derive(Parser)]
#[command(name = "w-vmm", version, about = "Local Apple Silicon ARM64 VMM")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Pause {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
    Resume {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
    Stop {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
    Status {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
    MemorySet {
        #[arg(long)]
        socket: std::path::PathBuf,
        #[arg(long)]
        requested_mib: u64,
    },
    MemoryStatus {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
    Run {
        /// Attach a disk; repeat for multiple disks. Bare paths get disk0, disk1, ...
        #[arg(long, value_name = "[NAME=]PATH")]
        disk: Vec<String>,
        #[arg(long, default_value_t = 512)]
        memory_mib: u64,
        #[arg(long, default_value_t = 1)]
        vcpus: u32,
        /// Extra memory capacity: positive multiple of max(128, block size) MiB.
        #[arg(long)]
        virtio_mem_size_mib: Option<u64>,
        /// Device block size in MiB (power of two); guest may adjust more coarsely.
        #[arg(long, default_value_t = 2, requires = "virtio_mem_size_mib")]
        virtio_mem_block_size_mib: u64,
        #[arg(long)]
        control_socket: Option<std::path::PathBuf>,
        /// Open all supplied disks read-only.
        #[arg(long, requires = "disk")]
        read_only: bool,
        /// Attach a macOS vmnet NAT interface (requires root or approved entitlement).
        #[arg(long)]
        net: bool,
    },
}

fn disk_paths(disks: Vec<String>) -> Result<BTreeMap<String, String>> {
    let mut paths = BTreeMap::new();
    for (index, value) in disks.into_iter().enumerate() {
        let (name, path) = match value.split_once('=') {
            Some((name, path)) => (name.to_owned(), path.to_owned()),
            None => (format!("disk{index}"), value),
        };
        if path.is_empty() {
            return Err(ArgumentError::EmptyDiskPath { name }.into());
        }
        if paths.contains_key(&name) {
            return Err(ArgumentError::DuplicateDisk { name }.into());
        }
        paths.insert(name, path);
    }
    Ok(paths)
}

fn run(command: Command) -> Result<()> {
    let command = match command {
        Command::Pause { socket } => return control_command(&socket, Request::Pause),
        Command::Resume { socket } => return control_command(&socket, Request::Resume),
        Command::Stop { socket } => return control_command(&socket, Request::Stop),
        Command::Status { socket } => return control_command(&socket, Request::Status),
        Command::MemorySet {
            socket,
            requested_mib,
        } => return control_command(&socket, Request::MemorySet { requested_mib }),
        Command::MemoryStatus { socket } => return control_command(&socket, Request::MemoryStatus),
        run => run,
    };
    let Command::Run {
        disk,
        memory_mib,
        vcpus,
        virtio_mem_size_mib,
        virtio_mem_block_size_mib,
        control_socket,
        read_only,
        net,
    } = command
    else {
        unreachable!()
    };
    let disks = disk_paths(disk)?
        .into_iter()
        .map(|(name, path)| Disk::open(Path::new(&path), read_only).map(|disk| (name, disk)))
        .collect::<std::result::Result<BTreeMap<_, _>, w_vmm::error::StorageError>>()?;
    let network = net.then(Vmnet::shared).transpose()?;
    if let Some(net) = &network {
        let mac = net.mac_address().map(|b| format!("{b:02x}")).join(":");
        eprintln!(
            "w-vmm: vmnet MAC {mac}, MTU {}; IPv4 {:?}",
            Vmnet::MTU,
            net.ipv4()
        );
    }
    let memory = virtio_mem_size_mib
        .map(|size| VirtioMem::new(size, virtio_mem_block_size_mib))
        .transpose()?;
    let (terminal, mut terminal_guard) = Terminal::new()?;
    let vmm = Vmm::new(
        VmConfig {
            memory_mib,
            vcpu_count: vcpus,
        },
        disks,
        network,
        memory,
        terminal,
    );
    terminal_guard.bind(vmm.control())?;
    let _control = control_socket
        .map(|path| Server::bind(&path, vmm.control()))
        .transpose()?;
    eprintln!("w-vmm: {vcpus} vCPU, {memory_mib} MiB; Ctrl-] exits");
    Ok(vmm.run()?)
}

fn control_command(socket: &Path, req: Request) -> Result<()> {
    let response = control::request(socket, req).unwrap_or_else(
        |e| serde_json::json!({"ok": false, "error": w_vmm::error::diagnostic(&e)}),
    );
    println!("{response}");
    if response["ok"] != true {
        return Err(ProtocolError::RequestFailed { response }.into());
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("w-vmm: {}", w_vmm::error::diagnostic(&e));
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_size_cli_requires_capacity_only_when_explicit() {
        assert!(Cli::try_parse_from(["w-vmm", "run"]).is_ok());
        assert!(Cli::try_parse_from(["w-vmm", "run", "--virtio-mem-block-size-mib", "2"]).is_err());
        let cli = Cli::try_parse_from(["w-vmm", "run", "--virtio-mem-size-mib", "128"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Run {
                virtio_mem_block_size_mib: 2,
                ..
            }
        ));
        let cli = Cli::try_parse_from([
            "w-vmm",
            "run",
            "--virtio-mem-size-mib",
            "128",
            "--virtio-mem-block-size-mib",
            "4",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Run {
                virtio_mem_block_size_mib: 4,
                ..
            }
        ));
    }

    #[test]
    fn named_disks_and_legacy_path() {
        let disks = disk_paths(vec!["sdb=b.qcow2".into(), "sda=a.qcow2".into()]).unwrap();
        assert_eq!(
            disks.keys().map(String::as_str).collect::<Vec<_>>(),
            ["sda", "sdb"]
        );
        assert_eq!(
            disk_paths(vec!["a.qcow2".into()]).unwrap()["disk0"],
            "a.qcow2"
        );
        assert!(disk_paths(vec!["sda=a".into(), "sda=b".into()]).is_err());
        assert!(disk_paths(vec!["sda=".into()]).is_err());
    }
}
