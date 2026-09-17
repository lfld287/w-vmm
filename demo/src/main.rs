use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use std::{collections::BTreeMap, path::Path};
use w_vmm::{
    VmConfig, Vmm,
    net::{NetDevice, macos::Vmnet},
    storage::Disk,
};
use w_vmm_demo::terminal::Terminal;

#[derive(Parser)]
#[command(name = "w-vmm", version, about = "Local Apple Silicon ARM64 VMM")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Run {
        /// Attach a disk; repeat for multiple disks. Bare paths get disk0, disk1, ...
        #[arg(long, value_name = "[NAME=]PATH")]
        disk: Vec<String>,
        #[arg(long, default_value_t = 512)]
        memory_mib: u64,
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
        ensure!(!path.is_empty(), "empty path for disk {name}");
        ensure!(!paths.contains_key(&name), "duplicate disk name: {name}");
        paths.insert(name, path);
    }
    Ok(paths)
}

fn run(command: Command) -> Result<()> {
    let Command::Run {
        disk,
        memory_mib,
        read_only,
        net,
    } = command;
    let disks = disk_paths(disk)?
        .into_iter()
        .map(|(name, path)| Disk::open(Path::new(&path), read_only).map(|disk| (name, disk)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let network = net.then(Vmnet::shared).transpose()?;
    if let Some(net) = &network {
        let mac = net.mac_address().map(|b| format!("{b:02x}")).join(":");
        eprintln!(
            "w-vmm: vmnet MAC {mac}, MTU {}; IPv4 {:?}",
            net.mtu(),
            net.ipv4()
        );
    }
    let terminal = Terminal::new()?;
    eprintln!("w-vmm: 1 vCPU, {memory_mib} MiB; Ctrl-] exits");
    Vmm::new(VmConfig { memory_mib }).run(disks, network, terminal)
}

fn main() -> std::process::ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("w-vmm: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
