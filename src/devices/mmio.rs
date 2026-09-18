//! Shared modern virtio-mmio transport and checked split-queue access.
use anyhow::{Result, ensure};
use std::sync::atomic::Ordering;
use virtio_bindings::bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_queue::{Queue, QueueOwnedT, QueueT, desc::split::Descriptor};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

pub(crate) const MAX_QUEUE: u16 = 128;

pub(crate) trait VirtioDevice {
    fn required_features(&self) -> u64 {
        0
    }

    fn generation(&self) -> u32 {
        0
    }

    fn device_id(&self) -> u32;

    fn features(&self) -> u64;

    fn queue_count(&self) -> usize;

    fn read_config(&self, offset: usize, data: &mut [u8]);

    fn notify(&mut self, queue: usize, queues: &mut Queues, mem: &GuestMemoryMmap) -> Result<()>;

    fn poll(&mut self, _queues: &mut Queues, _mem: &GuestMemoryMmap) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

pub(crate) struct Chain {
    pub(crate) head: u16,
    pub(crate) descriptors: Vec<Descriptor>,
}

pub(crate) struct Queues {
    pub(crate) rings: Vec<Queue>,
    pub(crate) negotiated: u64,
    interrupt: u32,
}

impl Queues {
    pub(crate) fn pinned(&self, mem: &GuestMemoryMmap) -> Result<Vec<(u64, u64)>> {
        let mut pins = Vec::new();
        for q in self.rings.iter().filter(|q| q.ready()) {
            ensure!(q.is_valid(mem), "invalid live queue");
            pins.extend([
                (q.desc_table(), u64::from(q.size()) * 16),
                (q.avail_ring(), 6 + u64::from(q.size()) * 2),
                (q.used_ring(), 6 + u64::from(q.size()) * 8),
            ]);
            let available = q
                .avail_idx(mem, Ordering::Acquire)?
                .0
                .wrapping_sub(q.next_avail());
            ensure!(available <= q.size(), "available ring overrun");
            for i in 0..available {
                let slot = q.next_avail().wrapping_add(i) % q.size();
                let mut index: u16 =
                    mem.read_obj(GuestAddress(q.avail_ring() + 4 + u64::from(slot) * 2))?;
                let mut ended = false;
                for _ in 0..q.size() {
                    ensure!(index < q.size(), "descriptor index out of range");
                    let d: Descriptor =
                        mem.read_obj(GuestAddress(q.desc_table() + u64::from(index) * 16))?;
                    ensure!(
                        d.flags() & !3 == 0 && mem.check_range(d.addr(), d.len() as usize),
                        "invalid pending descriptor"
                    );
                    if d.len() != 0 {
                        pins.push((d.addr().0, u64::from(d.len())));
                    }
                    if !d.has_next() {
                        ended = true;
                        break;
                    }
                    index = d.next();
                }
                ensure!(ended, "cyclic pending descriptor chain");
            }
        }
        Ok(pins)
    }

    pub(crate) fn available(&self, index: usize, mem: &GuestMemoryMmap) -> Result<u16> {
        let q = &self.rings[index];
        ensure!(q.is_valid(mem), "invalid virtqueue");
        let count = q
            .avail_idx(mem, Ordering::Acquire)?
            .0
            .wrapping_sub(q.next_avail());
        ensure!(count <= q.size(), "available ring overrun");
        Ok(count)
    }

    pub(crate) fn pop(&mut self, index: usize, mem: &GuestMemoryMmap) -> Result<Option<Chain>> {
        if self.available(index, mem)? == 0 {
            return Ok(None);
        }
        let q = &mut self.rings[index];
        let head = q
            .iter(mem)?
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid available descriptor"))?
            .head_index();
        let mut index = head;
        let mut descriptors = Vec::new();
        // Walk raw links before virtio-queue can expand an unnegotiated indirect table.
        // Bound traversal even for guest-created cycles, and validate before any I/O.
        for _ in 0..q.size() {
            ensure!(index < q.size(), "descriptor index out of range");
            let d: Descriptor =
                mem.read_obj(GuestAddress(q.desc_table() + u64::from(index) * 16))?;
            ensure!(
                !d.refers_to_indirect_table(),
                "unnegotiated indirect descriptor"
            );
            ensure!(
                d.flags() & !3 == 0 && mem.check_range(d.addr(), d.len() as usize),
                "invalid descriptor flags/range"
            );
            descriptors.push(d);
            if !d.has_next() {
                return Ok(Some(Chain { head, descriptors }));
            }
            index = d.next();
        }
        anyhow::bail!("cyclic descriptor chain")
    }

    pub(crate) fn complete(
        &mut self,
        index: usize,
        mem: &GuestMemoryMmap,
        head: u16,
        len: u32,
    ) -> Result<()> {
        self.rings[index].add_used(mem, head, len)?;
        // Suppression is advisory; EVENT_IDX is not advertised.
        self.interrupt |= 1;
        Ok(())
    }
}

pub(crate) struct Mmio<D: VirtioDevice> {
    pub(crate) device: D,
    pub(crate) queues: Queues,
    feature_sel: u32,
    driver_sel: u32,
    queue_sel: u32,
    pub(crate) features: u64,
    pub(crate) status: u32,
}

impl<D: VirtioDevice> Mmio<D> {
    pub(crate) fn new(device: D) -> Self {
        let features = device.features() | (1 << VIRTIO_F_VERSION_1);
        let rings = (0..device.queue_count())
            .map(|_| Queue::new(MAX_QUEUE).unwrap())
            .collect();
        Self {
            device,
            queues: Queues {
                rings,
                negotiated: 0,
                interrupt: 0,
            },
            feature_sel: 0,
            driver_sel: 0,
            queue_sel: 0,
            features,
            status: 0,
        }
    }

    pub(crate) fn interrupt_pending(&self) -> bool {
        self.queues.interrupt != 0
    }

    pub(crate) fn config_changed(&mut self) {
        self.queues.interrupt |= 2;
    }

    pub(crate) fn flush(&self) -> Result<()> {
        self.device.flush()
    }

    pub(crate) fn active(&self) -> bool {
        self.status & 0xcf == 0xf
    }

    pub(crate) fn poll(&mut self, mem: &GuestMemoryMmap) -> Result<()> {
        if self.active() {
            self.device.poll(&mut self.queues, mem)?;
        }
        Ok(())
    }

    pub(crate) fn read(&self, offset: u64, width: usize) -> u64 {
        if !matches!(width, 1 | 2 | 4 | 8) {
            return 0;
        }
        if (0x100..0x1000).contains(&offset) && offset + width as u64 <= 0x1000 {
            let mut data = [0; 8];
            self.device
                .read_config((offset - 0x100) as usize, &mut data[..width]);
            return u64::from_le_bytes(data);
        }
        if width != 4 {
            return 0;
        }
        match offset {
            0 => 0x74726976,
            4 => 2,
            8 => self.device.device_id() as u64,
            12 => 0x57564d4d,
            0x10 if self.feature_sel < 2 => {
                (self.features >> (self.feature_sel * 32)) & 0xffff_ffff
            }
            0x34 if (self.queue_sel as usize) < self.queues.rings.len() => MAX_QUEUE as u64,
            0x44 => self
                .queues
                .rings
                .get(self.queue_sel as usize)
                .is_some_and(QueueT::ready) as u64,
            0x60 => self.queues.interrupt as u64,
            0x70 => self.status as u64,
            0xfc => self.device.generation() as u64,
            _ => 0,
        }
    }

    pub(crate) fn write(&mut self, offset: u64, value: u32, mem: &GuestMemoryMmap) -> Result<()> {
        match offset {
            0x14 => self.feature_sel = value,
            0x24 => self.driver_sel = value,
            0x30 => self.queue_sel = value,
            0x20 if self.status & 8 == 0 && self.driver_sel < 2 => {
                let shift = self.driver_sel * 32;
                self.queues.negotiated =
                    (self.queues.negotiated & !(0xffff_ffff << shift)) | ((value as u64) << shift);
            }
            0x70 => {
                if value == 0 {
                    self.device.reset()?;
                    for q in &mut self.queues.rings {
                        q.reset();
                    }
                    self.queues.negotiated = 0;
                    self.queues.interrupt = 0;
                    self.feature_sel = 0;
                    self.driver_sel = 0;
                    self.queue_sel = 0;
                    self.status = 0;
                    return Ok(());
                }
                ensure!(
                    value & self.status == self.status,
                    "virtio status bits cannot be cleared without reset"
                );
                self.status = value;
                if value & 8 != 0
                    && (self.queues.negotiated & !self.features != 0
                        || self.queues.negotiated & (1 << VIRTIO_F_VERSION_1) == 0
                        || self.queues.negotiated & self.device.required_features()
                            != self.device.required_features())
                {
                    self.status &= !8;
                }
                if value & 4 != 0 {
                    ensure!(
                        self.status & 0xb == 0xb
                            && self.queues.rings.iter().all(|q| q.is_valid(mem)),
                        "virtio DRIVER_OK before valid negotiation/queues"
                    );
                }
            }
            0x64 => self.queues.interrupt &= !value,
            0x50 if self.active() && (value as usize) < self.queues.rings.len() => {
                self.device.notify(value as usize, &mut self.queues, mem)?;
            }
            0x38 | 0x44 | 0x80 | 0x84 | 0x90 | 0x94 | 0xa0 | 0xa4 => {
                if let Some(q) = self.queues.rings.get_mut(self.queue_sel as usize) {
                    ensure!(self.status & 4 == 0, "queue configuration after DRIVER_OK");
                    ensure!(
                        !q.ready() || (offset == 0x44 && value == 0),
                        "queue configuration while ready"
                    );
                    match offset {
                        0x38 => {
                            ensure!(value <= MAX_QUEUE as u32, "queue too large");
                            q.try_set_size(value as u16)?;
                        }
                        0x44 => {
                            ensure!(value <= 1, "invalid QueueReady");
                            q.set_ready(value == 1);
                            if value == 1 {
                                ensure!(q.is_valid(mem), "invalid queue memory");
                            }
                        }
                        0x80 => q.set_desc_table_address(Some(value), None),
                        0x84 => q.set_desc_table_address(None, Some(value)),
                        0x90 => q.set_avail_ring_address(Some(value), None),
                        0x94 => q.set_avail_ring_address(None, Some(value)),
                        0xa0 => q.set_used_ring_address(Some(value), None),
                        0xa4 => q.set_used_ring_address(None, Some(value)),
                        _ => unreachable!(),
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Copy a read-only device configuration, zero-filling unimplemented fields.
pub(crate) fn read_config(config: &[u8], offset: usize, data: &mut [u8]) {
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = config.get(offset + i).copied().unwrap_or(0);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn initialize<D: VirtioDevice>(device: &mut Mmio<D>, mem: &GuestMemoryMmap) {
        for (offset, value) in [
            (0x70, 1),
            (0x70, 3),
            (0x24, 1),
            (0x20, 1),
            (0x24, 0),
            (0x20, device.features as u32),
            (0x70, 11),
        ] {
            device.write(offset, value, mem).unwrap();
        }
        for index in 0..device.queues.rings.len() {
            let base = 0x1000 + index as u32 * 0x3000;
            for (offset, value) in [
                (0x30, index as u32),
                (0x38, 8),
                (0x80, base),
                (0x90, base + 0x1000),
                (0xa0, base + 0x2000),
                (0x44, 1),
            ] {
                device.write(offset, value, mem).unwrap();
            }
        }
        device.write(0x70, 15, mem).unwrap();
    }

    struct Fake {
        notifications: usize,
        resets: usize,
    }

    impl VirtioDevice for Fake {
        fn device_id(&self) -> u32 {
            42
        }

        fn features(&self) -> u64 {
            0
        }

        fn queue_count(&self) -> usize {
            2
        }

        fn read_config(&self, offset: usize, data: &mut [u8]) {
            read_config(&[0x12, 0x34, 0x56], offset, data);
        }

        fn notify(&mut self, _: usize, _: &mut Queues, _: &GuestMemoryMmap) -> Result<()> {
            self.notifications += 1;
            Ok(())
        }

        fn reset(&mut self) -> Result<()> {
            self.resets += 1;
            Ok(())
        }
    }

    fn setup() -> (Mmio<Fake>, GuestMemoryMmap) {
        (
            Mmio::new(Fake {
                notifications: 0,
                resets: 0,
            }),
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap(),
        )
    }

    #[test]
    fn registers_negotiation_and_reset() {
        let (mut d, mem) = setup();
        assert_eq!(d.read(0, 4), 0x74726976);
        assert_eq!(d.read(8, 4), 42);
        assert_eq!(d.read(0x100, 4), 0x563412);
        assert_eq!(d.read(0x101, 2), 0x5634);
        assert_eq!(d.read(u64::MAX, 8), 0);
        assert_eq!(d.read(0x100, 0), 0);
        d.write(0x50, 0, &mem).unwrap();
        assert_eq!(d.device.notifications, 0);
        initialize(&mut d, &mem);
        d.write(0x50, 1, &mem).unwrap();
        assert_eq!(d.device.notifications, 1);
        d.write(0x50, 2, &mem).unwrap();
        assert_eq!(d.device.notifications, 1);
        d.write(0x30, u32::MAX, &mem).unwrap();
        assert_eq!(d.read(0x34, 4), 0);
        assert_eq!(d.read(0x44, 4), 0);
        d.write(0x30, 0, &mem).unwrap();
        assert!(d.write(0x38, 4, &mem).is_err());
        assert!(d.write(0x70, 3, &mem).is_err());
        d.queues.complete(0, &mem, 0, 12).unwrap();
        d.queues.complete(1, &mem, 0, 0).unwrap();
        assert!(d.interrupt_pending());
        d.write(0x64, 1, &mem).unwrap();
        assert!(!d.interrupt_pending());
        d.write(0x70, 0, &mem).unwrap();
        assert_eq!(d.device.resets, 1);
        assert_eq!(d.queues.negotiated, 0);
        assert!(
            d.queues
                .rings
                .iter()
                .all(|q| !q.ready() && q.next_used() == 0)
        );
        initialize(&mut d, &mem);
        assert_eq!(d.status, 15);
    }

    #[test]
    fn rejects_unknown_features_and_unconfigured_second_queue() {
        let (mut d, mem) = setup();
        d.write(0x70, 3, &mem).unwrap();
        d.write(0x24, 1, &mem).unwrap();
        d.write(0x20, 3, &mem).unwrap(); // VERSION_1 plus an unsupported high feature.
        d.write(0x70, 11, &mem).unwrap();
        assert_eq!(d.status & 8, 0);
        assert!(d.write(0x70, 15, &mem).is_err());
        d.write(0x70, 0, &mem).unwrap();
        initialize(&mut d, &mem);
        d.status = 11;
        d.queues.rings[1].set_ready(false);
        assert!(d.write(0x70, 15, &mem).is_err());
    }

    #[test]
    fn queue_memory_and_descriptor_validation() {
        for (flags, next, addr) in [
            (1, 9, 0x8000),
            (1, 0, 0x8000),
            (4, 0, 0x8000),
            (8, 0, 0x8000),
            (0, 0, u64::MAX),
        ] {
            let (mut d, mem) = setup();
            initialize(&mut d, &mem);
            mem.write_obj(Descriptor::new(addr, 16, flags, next), GuestAddress(0x1000))
                .unwrap();
            mem.write_obj(1u16, GuestAddress(0x2002)).unwrap();
            assert!(d.queues.pop(0, &mem).is_err());
        }
        let (mut d, mem) = setup();
        initialize(&mut d, &mem);
        mem.write_obj(9u16, GuestAddress(0x2002)).unwrap();
        assert!(d.queues.available(0, &mem).is_err());
        d.write(0x70, 0, &mem).unwrap();
        assert!(d.write(0x38, 129, &mem).is_err());
        assert!(d.write(0x38, 3, &mem).is_err());
        d.write(0x80, 0xffff_f000, &mem).unwrap();
        assert!(d.write(0x44, 1, &mem).is_err());
    }
}
