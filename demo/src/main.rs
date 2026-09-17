use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "w-vmm", version, about = "Local Apple Silicon ARM64 VMM")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Run {
        #[arg(long)]
        disk: Option<PathBuf>,
        #[arg(long, default_value_t = 512)]
        memory_mib: u64,
        #[arg(long, requires = "disk")]
        read_only: bool,
    },
}

fn main() -> std::process::ExitCode {
    let Cli {
        command:
            Command::Run {
                disk,
                memory_mib,
                read_only,
            },
    } = Cli::parse();
    match w_vmm::Vmm::new(w_vmm::VmConfig {
        disk,
        memory_mib,
        read_only,
    })
    .run()
    {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("w-vmm: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
