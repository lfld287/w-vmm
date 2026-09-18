use anyhow::{Result, ensure};
use linux_loader::loader::{KernelLoader, pe::PE};
use std::io::Cursor;
use vm_fdt::FdtWriter;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub const RAM: u64 = 0x4000_0000;
pub const UART: u64 = 0x0900_0000;
pub const VIRTIO_BASE: u64 = 0x0a00_0000;
pub const VIRTIO_STRIDE: u64 = 0x1000;
pub const GIC_DIST: u64 = 0x0800_0000;
pub const GIC_REDIST: u64 = 0x1000_0000;
pub const UART_IRQ: u32 = 33;
pub const VIRTIO_IRQ_BASE: u32 = 34;
pub const KERNEL: &[u8] = include_bytes!("../assets/Image");
pub const INITRD: &[u8] = include_bytes!("../assets/initramfs.cpio.gz");

pub(crate) const MIB: u64 = 1 << 20;

pub(crate) fn mib_bytes(mib: u64) -> Result<u64> {
    mib.checked_mul(MIB)
        .ok_or_else(|| anyhow::anyhow!("memory capacity conversion overflow"))
}

pub(crate) fn validate_ipa_range(start: u64, size: u64, bits: u32) -> Result<()> {
    ensure!((1..=64).contains(&bits), "invalid HVF IPA width {bits}");
    let end = start
        .checked_add(size)
        .ok_or_else(|| anyhow::anyhow!("memory address overflow"))?;
    ensure!(
        bits == 64 || end <= (1u64 << bits),
        "memory range {start:#x}..{end:#x} exceeds HVF {bits}-bit IPA range"
    );
    Ok(())
}

#[derive(Debug)]
pub struct Layout {
    pub size: usize,
    pub entry: u64,
    pub initrd: u64,
    pub dtb: u64,
}

impl Layout {
    pub fn new(mib: u64, kernel: &[u8], initrd_len: usize) -> Result<Self> {
        ensure!(mib >= 128, "memory must be at least 128 MiB");
        ensure!(
            kernel.len() >= 64 && &kernel[56..60] == b"ARM\x64",
            "invalid ARM64 Image"
        );
        let word = |p| u64::from_le_bytes(kernel[p..p + 8].try_into().unwrap());
        let image_size = word(16);
        ensure!(
            image_size != 0,
            "legacy Image without image_size is unsupported"
        );
        ensure!(word(24) & 1 == 0, "big endian kernel unsupported");
        let offset = word(8);
        ensure!(offset.is_multiple_of(4096), "unaligned kernel text offset");
        let entry = RAM
            .checked_add(offset)
            .ok_or_else(|| anyhow::anyhow!("kernel offset overflow"))?;
        let bytes = mib_bytes(mib)?;
        let size = usize::try_from(bytes)?;
        ensure!(
            size <= isize::MAX as usize,
            "RAM exceeds host allocation size"
        );
        let end = RAM
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("RAM address overflow"))?;
        let dtb = end - 0x20_0000;
        let initrd = dtb
            .checked_sub(initrd_len as u64)
            .ok_or_else(|| anyhow::anyhow!("initrd too large"))?
            & !0xffff;
        let kernel_end = entry
            .checked_add(image_size.max(kernel.len() as u64))
            .ok_or_else(|| anyhow::anyhow!("Image size overflow"))?;
        ensure!(
            initrd >= RAM && kernel_end <= initrd,
            "kernel/initramfs/FDT overlap or exceed RAM"
        );
        Ok(Self {
            size,
            entry,
            initrd,
            dtb,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioRegion {
    pub address: u64,
    pub irq: u32,
}

pub fn virtio_regions(count: usize) -> Result<Vec<VirtioRegion>> {
    ensure!(
        count <= (1020 - VIRTIO_IRQ_BASE) as usize,
        "too many virtio devices for GIC SPIs"
    );
    Ok((0..count)
        .map(|i| VirtioRegion {
            address: VIRTIO_BASE + i as u64 * VIRTIO_STRIDE,
            irq: VIRTIO_IRQ_BASE + i as u32,
        })
        .collect())
}

pub fn fdt(
    l: &Layout,
    devices: &[VirtioRegion],
    redist_size: u64,
    timers: [u32; 2],
    vcpu_count: u32,
    virtio_mem: bool,
) -> Result<Vec<u8>> {
    ensure!(vcpu_count > 0, "at least one CPU required");
    let mut f = FdtWriter::new()?;
    let root = f.begin_node("")?;
    f.property_string("compatible", "w-vmm,arm64")?;
    f.property_u32("#address-cells", 2)?;
    f.property_u32("#size-cells", 2)?;
    f.property_u32("interrupt-parent", 1)?;
    let n = f.begin_node("chosen")?;
    f.property_string(
        "bootargs",
        &format!("console=ttyS0,115200 earlycon=uart8250,mmio,0x09000000 rdinit=/init panic=-1{}", if virtio_mem { " memory_hotplug.online_policy=auto-movable memory_hotplug.auto_movable_ratio=301 memhp_default_state=online" } else { "" }),
    )?;
    f.property_string("stdout-path", "/serial@9000000")?;
    f.property_u64("linux,initrd-start", l.initrd)?;
    f.property_u64("linux,initrd-end", l.initrd + INITRD.len() as u64)?;
    f.end_node(n)?;
    let n = f.begin_node("memory@40000000")?;
    f.property_string("device_type", "memory")?;
    f.property_array_u64("reg", &[RAM, l.size as u64])?;
    f.end_node(n)?;
    let n = f.begin_node("cpus")?;
    f.property_u32("#address-cells", 1)?;
    f.property_u32("#size-cells", 0)?;
    for id in 0..vcpu_count {
        let cpu = f.begin_node(&format!("cpu@{id:x}"))?;
        f.property_string("device_type", "cpu")?;
        f.property_string("compatible", "arm,arm-v8")?;
        f.property_u32("reg", id)?;
        f.property_string("enable-method", "psci")?;
        f.end_node(cpu)?;
    }
    f.end_node(n)?;
    let n = f.begin_node("psci")?;
    f.property_string("compatible", "arm,psci-0.2")?;
    f.property_string("method", "hvc")?;
    f.end_node(n)?;
    let n = f.begin_node("interrupt-controller@8000000")?;
    f.property_string("compatible", "arm,gic-v3")?;
    f.property_null("interrupt-controller")?;
    f.property_u32("#interrupt-cells", 3)?;
    f.property_u32("phandle", 1)?;
    f.property_array_u64("reg", &[GIC_DIST, 0x10000, GIC_REDIST, redist_size])?;
    f.end_node(n)?;
    let n = f.begin_node("timer")?;
    f.property_string("compatible", "arm,armv8-timer")?;
    f.property_null("always-on")?;
    f.property_array_u32(
        "interrupts",
        &[
            1,
            13,
            4,
            1,
            timers[0] - 16,
            4,
            1,
            timers[1] - 16,
            4,
            1,
            10,
            4,
        ],
    )?;
    f.end_node(n)?;
    let n = f.begin_node("serial@9000000")?;
    f.property_string("compatible", "ns16550a")?;
    f.property_array_u64("reg", &[UART, 0x1000])?;
    f.property_u32("clock-frequency", 1843200)?;
    f.property_u32("reg-io-width", 1)?;
    f.property_array_u32("interrupts", &[0, UART_IRQ - 32, 4])?;
    f.end_node(n)?;
    for device in devices {
        let n = f.begin_node(&format!("virtio_mmio@{:x}", device.address))?;
        f.property_string("compatible", "virtio,mmio")?;
        f.property_array_u64("reg", &[device.address, VIRTIO_STRIDE])?;
        f.property_array_u32("interrupts", &[0, device.irq - 32, 4])?;
        f.property_null("dma-coherent")?;
        f.end_node(n)?;
    }
    f.end_node(root)?;
    Ok(f.finish()?)
}

pub fn load(mem: &GuestMemoryMmap, l: &Layout, dtb: &[u8]) -> Result<()> {
    ensure!(dtb.len() <= 0x20_0000, "FDT exceeds reserved space");
    let result = PE::load(mem, Some(GuestAddress(RAM)), &mut Cursor::new(KERNEL), None)?;
    ensure!(
        result.kernel_load.0 == l.entry,
        "loader/layout disagreement"
    );
    mem.write_slice(INITRD, GuestAddress(l.initrd))?;
    mem.write_slice(dtb, GuestAddress(l.dtb))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_dtb() {
        let l = Layout::new(512, KERNEL, INITRD.len()).unwrap();
        assert!(l.entry < l.initrd && l.initrd < l.dtb);
        let dtb = fdt(&l, &virtio_regions(1).unwrap(), 0x20000, [30, 27], 1, false).unwrap();
        assert_eq!(&dtb[..4], &0xd00dfeedu32.to_be_bytes());
        assert!(dtb.windows(12).any(|w| w == b"virtio,mmio\0"));
    }

    #[test]
    fn cpu_topology_and_base_only_memory() {
        let l = Layout::new(512, KERNEL, INITRD.len()).unwrap();
        let dtb = fdt(&l, &virtio_regions(1).unwrap(), 0x80000, [30, 27], 4, true).unwrap();
        for id in 0..4 {
            let name = format!("cpu@{id:x}\0");
            assert!(dtb.windows(name.len()).any(|w| w == name.as_bytes()));
        }
        assert_eq!(dtb.windows(7).filter(|w| *w == b"memory@").count(), 1);
        assert!(dtb.windows(12).any(|w| w == b"auto-movable"));
        assert!(fdt(&l, &[], 0x80000, [30, 27], 0, false).is_err());
    }

    #[test]
    fn large_ram_and_ipa_boundaries() {
        assert_eq!(
            Layout::new(32768, KERNEL, INITRD.len()).unwrap().size,
            32768 * MIB as usize
        );
        for mib in [u64::MAX, u64::MAX / MIB + 1, u64::MAX / MIB] {
            assert!(Layout::new(mib, KERNEL, 0).is_err());
        }
        let limit = 1u64 << 36;
        validate_ipa_range(RAM, limit - RAM, 36).unwrap();
        assert!(validate_ipa_range(RAM, limit - RAM + 1, 36).is_err());
        assert!(validate_ipa_range(u64::MAX, 1, 64).is_err());
        for bits in [0, 65] {
            assert!(validate_ipa_range(RAM, MIB, bits).is_err());
        }
        validate_ipa_range(RAM, MIB, 64).unwrap();
    }

    #[test]
    fn rejects_layout_overflow() {
        assert!(Layout::new(64, KERNEL, INITRD.len()).is_err());
        assert!(Layout::new(128, KERNEL, usize::MAX).is_err());
        let mut k = KERNEL[..64].to_vec();
        k[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Layout::new(512, &k, 0).is_err());
    }

    #[test]
    fn multiple_device_regions_and_fdt() {
        let layout = Layout::new(512, KERNEL, INITRD.len()).unwrap();
        for count in [0, 1, 2, 3] {
            let regions = virtio_regions(count).unwrap();
            let dtb = fdt(&layout, &regions, 0x20000, [30, 27], 1, false).unwrap();
            assert_eq!(
                dtb.windows(12).filter(|w| *w == b"virtio_mmio@").count(),
                count
            );
            for (i, region) in regions.iter().enumerate() {
                assert_eq!(region.address, VIRTIO_BASE + i as u64 * VIRTIO_STRIDE);
                assert_eq!(region.irq, VIRTIO_IRQ_BASE + i as u32);
                assert!(region.address + VIRTIO_STRIDE <= GIC_REDIST);
                let node = format!("virtio_mmio@{:x}\0", region.address);
                assert!(dtb.windows(node.len()).any(|w| w == node.as_bytes()));
            }
        }
        assert!(virtio_regions(usize::MAX).is_err());
    }
}
