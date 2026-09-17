#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("w-vmm requires Apple Silicon macOS 15+");
pub mod boot;
mod devices;
mod hvf;
pub mod storage;
mod terminal;
use anyhow::{Result, bail, ensure};
use devices::block::Block;
use std::{path::PathBuf, sync::atomic::Ordering};
use vm_memory::{GuestAddress, GuestMemoryMmap};
use vm_superio::{Serial, Trigger};

#[derive(Debug, Clone)]
pub struct VmConfig {
    pub disk: Option<PathBuf>,
    pub memory_mib: u64,
    pub read_only: bool,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            disk: None,
            memory_mib: 512,
            read_only: false,
        }
    }
}

pub struct Vmm {
    config: VmConfig,
}

struct Irq;

impl Trigger for Irq {
    type E = std::io::Error;

    fn trigger(&self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Vmm {
    pub fn new(config: VmConfig) -> Self {
        Self { config }
    }

    pub fn run(self) -> Result<()> {
        let layout = boot::Layout::new(self.config.memory_mib, boot::KERNEL, boot::INITRD.len())?;
        let mut block = self
            .config
            .disk
            .as_ref()
            .map(|p| storage::Disk::open(p, self.config.read_only).map(|d| Block::new(Box::new(d))))
            .transpose()?;
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(boot::RAM), layout.size)])?;
        let vm = hvf::Vm::new(mem)?;
        let (redist, timers) = vm.gic()?;
        let dtb = boot::fdt(&layout, block.is_some(), redist, timers)?;
        boot::load(vm.memory(), &layout, &dtb)?;
        let cpu = hvf::Vcpu::new(&vm, layout.entry, layout.dtb)?;
        let terminal = terminal::Terminal::new()?;
        let _kicker = terminal::Kicker::new(cpu.id);
        let mut serial = Serial::new(Irq, std::io::stdout());
        eprintln!(
            "w-vmm: 1 vCPU, {} MiB; Ctrl-] exits",
            self.config.memory_mib
        );
        let result = (|| -> Result<()> {
            loop {
                if terminal.stop.load(Ordering::Relaxed) {
                    break;
                }
                let input = terminal.input(serial.fifo_capacity())?;
                if !input.is_empty() {
                    serial.enqueue_raw_bytes(&input)?;
                }
                hvf::spi(
                    boot::UART_IRQ,
                    serial.state().interrupt_identification & 1 == 0,
                )?;
                if let Some(b) = &block {
                    hvf::spi(boot::BLOCK_IRQ, b.interrupt != 0)?;
                }
                let exit = cpu.run()?;
                match exit.reason {
                    0 => continue,
                    1 => {
                        let e = exit.exception;
                        let esr = e.syndrome;
                        match esr >> 26 {
                            0x16 => {
                                let function = cpu.get(0)?;
                                match function {
                                    0x84000008 | 0x84000009 => {
                                        eprintln!(
                                            "w-vmm: guest requested {}",
                                            if function == 0x84000008 {
                                                "poweroff"
                                            } else {
                                                "reset (exit)"
                                            }
                                        );
                                        break;
                                    }
                                    0x84000000 => cpu.set(0, 2)?,
                                    0x84000006 => cpu.set(0, 2)?,
                                    _ => cpu.set(0, u64::MAX)?,
                                }

                                // HVC returns PC after the trapping instruction.
                            }
                            0x24 => {
                                ensure!(
                                    esr & (1 << 24) != 0 && esr & ((1 << 7) | (1 << 8)) == 0,
                                    "unsupported data abort: {e:?}"
                                );
                                let width = 1usize << ((esr >> 22) & 3);
                                let reg = ((esr >> 16) & 31) as u32;
                                let write = esr & (1 << 6) != 0;
                                let addr = e.physical_address;
                                let value = if write && reg != 31 { cpu.get(reg)? } else { 0 };
                                let read = if (boot::UART..boot::UART + 8).contains(&addr)
                                    && width == 1
                                {
                                    let offset = (addr - boot::UART) as u8;
                                    if write {
                                        serial.write(offset, value as u8)?;
                                        0
                                    } else {
                                        serial.read(offset) as u64
                                    }
                                } else if (boot::BLOCK..boot::BLOCK + 0x1000).contains(&addr) {
                                    let b = block.as_mut().ok_or_else(|| {
                                        anyhow::anyhow!("access to absent block device")
                                    })?;
                                    if write {
                                        ensure!(width == 4, "virtio MMIO writes must be 32 bit");
                                        b.write(addr - boot::BLOCK, value as u32, vm.memory())?;
                                        0
                                    } else {
                                        b.read(addr - boot::BLOCK, width)
                                    }
                                } else {
                                    bail!(
                                        "unmapped MMIO {addr:#x}, size {width}, PC={:#x}",
                                        cpu.get(31)?
                                    )
                                };
                                if !write && reg != 31 {
                                    let mut read = read;
                                    if esr & (1 << 21) != 0 {
                                        read = ((read << (64 - width * 8)) as i64
                                            >> (64 - width * 8))
                                            as u64;
                                    }
                                    if esr & (1 << 15) == 0 {
                                        read &= 0xffff_ffff;
                                    }
                                    cpu.set(reg, read)?;
                                }
                                cpu.advance()?;
                            }
                            0x18 => {
                                let sysreg = (((esr >> 20) & 3) << 14)
                                    | (((esr >> 14) & 7) << 11)
                                    | (((esr >> 10) & 15) << 7)
                                    | (((esr >> 1) & 15) << 3)
                                    | ((esr >> 17) & 7);
                                // No virtual external debugger: OS lock and double lock are RAZ/WI.
                                ensure!(
                                    matches!(sysreg, 0x8084 | 0x808c | 0x809c),
                                    "unsupported sysreg {sysreg:#x}, ESR={esr:#x}"
                                );
                                let rt = ((esr >> 5) & 31) as u32;
                                if esr & 1 != 0 && rt != 31 {
                                    cpu.set(rt, 0)?;
                                }
                                cpu.advance()?;
                            }
                            1 => cpu.advance()?, // WFI/WFE; next run or host kicker resumes.
                            _ => bail!("unhandled HVF exception {e:?}, PC={:#x}", cpu.get(31)?),
                        }
                    }
                    _ => bail!("unexpected HVF exit {exit:?}; native GIC should handle timers"),
                }
            }
            Ok(())
        })();
        let flushed = block.as_mut().map(|b| b.disk.flush()).transpose();
        result?;
        flushed?;
        Ok(())
    }
}
