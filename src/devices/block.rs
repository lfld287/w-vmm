use crate::storage::{BlockStorage, bounds};
use anyhow::{Result, ensure};
use std::sync::atomic::Ordering;
use virtio_bindings::bindings::{virtio_blk::*, virtio_config::VIRTIO_F_VERSION_1};
use virtio_queue::{DescriptorChain, Queue, QueueOwnedT, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

const MAX_QUEUE: u16 = 128;
const MAX_REQUEST: usize = 1024 * 1024;

pub struct Block<BS: BlockStorage> {
    disk: BS,
    queue: Queue,
    feature_sel: u32,
    driver_sel: u32,
    queue_sel: u32,
    features: u64,
    negotiated: u64,
    status: u32,
    interrupt: u32,
}

impl<BS: BlockStorage> Block<BS> {
    pub fn new(disk: BS) -> Self {
        let features = (1u64 << VIRTIO_F_VERSION_1)
            | (1 << VIRTIO_BLK_F_FLUSH)
            | if disk.read_only() {
                1 << VIRTIO_BLK_F_RO
            } else {
                0
            };
        Self {
            disk,
            queue: Queue::new(MAX_QUEUE).unwrap(),
            feature_sel: 0,
            driver_sel: 0,
            queue_sel: 0,
            features,
            negotiated: 0,
            status: 0,
            interrupt: 0,
        }
    }

    pub(crate) fn interrupt_pending(&self) -> bool {
        self.interrupt != 0
    }

    pub(crate) fn flush(&self) -> Result<()> {
        self.disk.flush()
    }

    pub fn read(&self, offset: u64, width: usize) -> u64 {
        if offset >= 0x100 && offset + width as u64 <= 0x108 {
            return ((self.disk.size() / 512) >> ((offset - 0x100) * 8))
                & (u64::MAX >> ((8 - width) * 8));
        }
        if width != 4 {
            return 0;
        }
        match offset {
            0 => 0x74726976,
            4 => 2,
            8 => 2,
            12 => 0x57564d4d,
            0x10 => {
                if self.feature_sel < 2 {
                    self.features >> (self.feature_sel * 32) & 0xffff_ffff
                } else {
                    0
                }
            }
            0x34 => {
                if self.queue_sel == 0 {
                    MAX_QUEUE as u64
                } else {
                    0
                }
            }
            0x44 => u64::from(self.queue_sel == 0 && self.queue.ready()),
            0x60 => self.interrupt as u64,
            0x70 => self.status as u64,
            0xfc => 0,
            _ => 0,
        }
    }

    fn reset(&mut self) -> Result<()> {
        self.disk.flush()?;
        self.queue.reset();
        self.feature_sel = 0;
        self.driver_sel = 0;
        self.queue_sel = 0;
        self.negotiated = 0;
        self.status = 0;
        self.interrupt = 0;
        Ok(())
    }

    pub fn write(&mut self, offset: u64, value: u32, mem: &GuestMemoryMmap) -> Result<()> {
        match offset {
            0x14 => self.feature_sel = value,
            0x24 => self.driver_sel = value,
            0x30 => self.queue_sel = value,
            0x20 if self.status & 8 == 0 && self.driver_sel < 2 => {
                let shift = self.driver_sel * 32;
                self.negotiated =
                    (self.negotiated & !(0xffff_ffff << shift)) | ((value as u64) << shift);
            }
            0x70 => {
                if value == 0 {
                    return self.reset();
                }
                ensure!(
                    value & self.status == self.status,
                    "virtio status bits cannot be cleared without reset"
                );
                self.status = value;
                if value & 8 != 0
                    && (self.negotiated & !self.features != 0 || self.negotiated & (1 << 32) == 0)
                {
                    self.status &= !8;
                }
                if value & 4 != 0 {
                    ensure!(
                        self.status & 0xb == 0xb && self.queue.is_valid(mem),
                        "virtio DRIVER_OK before valid negotiation/queue"
                    );
                }
            }
            0x64 => self.interrupt &= !value,
            0x50 if value == 0 && self.status & 0x8f == 0xf => self.process(mem)?,
            0x38 | 0x44 | 0x80 | 0x84 | 0x90 | 0x94 | 0xa0 | 0xa4 if self.queue_sel == 0 => {
                ensure!(self.status & 4 == 0, "queue configuration after DRIVER_OK");
                match offset {
                    0x38 => {
                        ensure!(value <= MAX_QUEUE as u32, "queue too large");
                        self.queue.try_set_size(value as u16)?;
                    }
                    0x44 => {
                        ensure!(value <= 1, "invalid QueueReady");
                        self.queue.set_ready(value == 1);
                        if value == 1 {
                            ensure!(self.queue.is_valid(mem), "invalid queue memory");
                        }
                    }
                    0x80 => self.queue.set_desc_table_address(Some(value), None),
                    0x84 => self.queue.set_desc_table_address(None, Some(value)),
                    0x90 => self.queue.set_avail_ring_address(Some(value), None),
                    0x94 => self.queue.set_avail_ring_address(None, Some(value)),
                    0xa0 => self.queue.set_used_ring_address(Some(value), None),
                    0xa4 => self.queue.set_used_ring_address(None, Some(value)),
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn process(&mut self, mem: &GuestMemoryMmap) -> Result<()> {
        ensure!(self.queue.is_valid(mem), "invalid virtqueue");
        let count = self
            .queue
            .avail_idx(mem, Ordering::Acquire)?
            .0
            .wrapping_sub(self.queue.next_avail());
        ensure!(count <= self.queue.size(), "available ring overrun");
        for _ in 0..count {
            let chain = self
                .queue
                .iter(mem)?
                .next()
                .ok_or_else(|| anyhow::anyhow!("invalid available descriptor"))?;
            let head = chain.head_index();
            // Validate raw links before virtio-queue expands indirect tables. We do not
            // advertise INDIRECT_DESC; also bound every traversal to the queue size.
            let mut index = head;
            let mut terminated = false;
            for _ in 0..self.queue.size() {
                ensure!(index < self.queue.size(), "descriptor index out of range");
                let d: virtio_queue::desc::split::Descriptor = mem.read_obj(GuestAddress(
                    self.queue.desc_table() + u64::from(index) * 16,
                ))?;
                ensure!(
                    !d.refers_to_indirect_table(),
                    "unnegotiated indirect descriptor"
                );
                if !d.has_next() {
                    terminated = true;
                    break;
                }
                index = d.next();
            }
            ensure!(terminated, "cyclic descriptor chain");
            let len = self.request(mem, chain)?;
            self.queue.add_used(mem, head, len)?;
            // Interrupt suppression is advisory; always notifying is valid without EVENT_IDX.
            self.interrupt |= 1;
        }
        Ok(())
    }

    fn request(
        &self,
        mem: &GuestMemoryMmap,
        chain: DescriptorChain<&GuestMemoryMmap>,
    ) -> Result<u32> {
        let desc: Vec<_> = chain.collect();
        ensure!(
            desc.len() >= 2 && !desc.last().unwrap().has_next(),
            "truncated/cyclic descriptor chain"
        );
        ensure!(
            desc.iter().all(|d| !d.refers_to_indirect_table()
                && d.flags() & !3 == 0
                && mem.check_range(d.addr(), d.len() as usize)),
            "invalid descriptor flags/range"
        );
        let header = desc[0];
        let status = desc[desc.len() - 1];
        ensure!(
            !header.is_write_only()
                && header.len() == 16
                && status.is_write_only()
                && status.len() == 1,
            "invalid block header/status"
        );
        let mut h = [0; 16];
        mem.read_slice(&mut h, header.addr())?;
        let kind = u32::from_le_bytes(h[..4].try_into().unwrap());
        let sector = u64::from_le_bytes(h[8..].try_into().unwrap());
        let data = &desc[1..desc.len() - 1];
        let total = data
            .iter()
            .try_fold(0usize, |sum, d| sum.checked_add(d.len() as usize))
            .ok_or_else(|| anyhow::anyhow!("request length overflow"))?;
        ensure!(total <= MAX_REQUEST, "block request exceeds 1 MiB limit");
        let mut used = 1;
        let result = (|| -> Result<u8> {
            match kind {
                VIRTIO_BLK_T_IN | VIRTIO_BLK_T_OUT => {
                    let read = kind == VIRTIO_BLK_T_IN;
                    ensure!(
                        data.iter().all(|d| d.is_write_only() == read),
                        "block descriptor direction mismatch"
                    );
                    ensure!(total.is_multiple_of(512), "unaligned block request");
                    ensure!(read || !self.disk.read_only(), "write to read-only disk");
                    let offset = sector
                        .checked_mul(512)
                        .ok_or_else(|| anyhow::anyhow!("sector overflow"))?;
                    bounds(self.disk.size(), offset, total)?;
                    let mut buf = vec![0; total];
                    if read {
                        self.disk.read(offset, &mut buf)?;
                        let mut p = 0;
                        for d in data {
                            let n = d.len() as usize;
                            mem.write_slice(&buf[p..p + n], d.addr())?;
                            p += n;
                        }
                        used += total as u32;
                    } else {
                        let mut p = 0;
                        for d in data {
                            let n = d.len() as usize;
                            mem.read_slice(&mut buf[p..p + n], d.addr())?;
                            p += n;
                        }
                        self.disk.write(offset, &buf)?;
                        if self.negotiated & (1 << VIRTIO_BLK_F_FLUSH) == 0 {
                            self.disk.flush()?; // Without FLUSH negotiation, use write-through.
                        }
                    }
                }
                VIRTIO_BLK_T_FLUSH => {
                    ensure!(
                        total == 0 && self.negotiated & (1 << VIRTIO_BLK_F_FLUSH) != 0,
                        "invalid FLUSH"
                    );
                    self.disk.flush()?;
                }
                VIRTIO_BLK_T_GET_ID => {
                    ensure!(
                        total >= 20 && data.iter().all(|d| d.is_write_only()),
                        "invalid GET_ID"
                    );
                    let id = b"w-vmm-data-000000001";
                    let mut p = 0;
                    for d in data {
                        let n = (d.len() as usize).min(20 - p);
                        mem.write_slice(&id[p..p + n], d.addr())?;
                        p += n;
                        if p == 20 {
                            break;
                        }
                    }
                    used += 20;
                }
                _ => return Ok(VIRTIO_BLK_S_UNSUPP as u8),
            }
            Ok(VIRTIO_BLK_S_OK as u8)
        })();
        let status_value = match result {
            Ok(s) => s,
            Err(e) => {
                eprintln!("virtio-blk request {kind}: {e:#}");
                VIRTIO_BLK_S_IOERR as u8
            }
        };
        mem.write_obj(status_value, status.addr())?;
        Ok(used)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use virtio_queue::desc::split::Descriptor;
    use vm_memory::GuestAddress;

    struct Fake {
        data: RefCell<Vec<u8>>,
        ro: bool,
        fail: bool,
        flushes: Cell<u32>,
    }

    impl BlockStorage for Rc<Fake> {
        fn size(&self) -> u64 {
            self.data.borrow().len() as u64
        }

        fn read_only(&self) -> bool {
            self.ro
        }

        fn read(&self, o: u64, d: &mut [u8]) -> Result<()> {
            ensure!(!self.fail, "injected host I/O error");
            bounds(self.size(), o, d.len())?;
            d.copy_from_slice(&self.data.borrow()[o as usize..o as usize + d.len()]);
            Ok(())
        }

        fn write(&self, o: u64, d: &[u8]) -> Result<()> {
            ensure!(!self.ro && !self.fail, "read-only/injected host I/O error");
            bounds(self.size(), o, d.len())?;
            self.data.borrow_mut()[o as usize..o as usize + d.len()].copy_from_slice(d);
            Ok(())
        }

        fn flush(&self) -> Result<()> {
            self.flushes.set(self.flushes.get() + 1);
            ensure!(!self.fail, "injected sync error");
            Ok(())
        }
    }

    fn setup(ro: bool, fail: bool) -> (Block<Rc<Fake>>, GuestMemoryMmap, Rc<Fake>) {
        let disk = Rc::new(Fake {
            data: RefCell::new(vec![0; 4096]),
            ro,
            fail,
            flushes: Cell::new(0),
        });
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut b = Block::new(disk.clone());
        for (o, v) in [
            (0x70, 1),
            (0x70, 3),
            (0x24, 1),
            (0x20, 1),
            (0x24, 0),
            (0x20, (b.features & 0xffff_ffff) as u32),
            (0x70, 11),
            (0x38, 8),
            (0x80, 0x1000),
            (0x90, 0x2000),
            (0xa0, 0x3000),
            (0x44, 1),
            (0x70, 15),
        ] {
            b.write(o, v, &mem).unwrap();
        }
        (b, mem, disk)
    }

    fn submit(
        b: &mut Block<Rc<Fake>>,
        m: &GuestMemoryMmap,
        kind: u32,
        sector: u64,
        len: u32,
    ) -> Result<()> {
        let idx = b.queue.next_avail();
        let header = Descriptor::new(0x4000, 16, 1, 1);
        let data = Descriptor::new(0x5000, len, if kind == VIRTIO_BLK_T_OUT { 1 } else { 3 }, 2);
        let status = Descriptor::new(0x6000, 1, 2, 0);
        m.write_obj(header, GuestAddress(0x1000))?;
        m.write_obj(data, GuestAddress(0x1010))?;
        m.write_obj(status, GuestAddress(0x1020))?;
        m.write_obj(kind.to_le(), GuestAddress(0x4000))?;
        m.write_obj(sector.to_le(), GuestAddress(0x4008))?;
        m.write_obj(0u16, GuestAddress(0x2004 + 2 * (idx % 8) as u64))?;
        m.write_obj(idx.wrapping_add(1).to_le(), GuestAddress(0x2002))?;
        b.write(0x50, 0, m)
    }

    fn status(m: &GuestMemoryMmap) -> u8 {
        m.read_obj(GuestAddress(0x6000)).unwrap()
    }

    #[test]
    fn io_used_ring_flush_and_reset() {
        let (mut b, m, d) = setup(false, false);
        m.write_slice(&[0x5a; 512], GuestAddress(0x5000)).unwrap();
        submit(&mut b, &m, VIRTIO_BLK_T_OUT, 1, 512).unwrap();
        assert_eq!(status(&m), 0);
        assert_eq!(&d.data.borrow()[512..1024], &[0x5a; 512]);
        m.write_slice(&[0; 512], GuestAddress(0x5000)).unwrap();
        submit(&mut b, &m, VIRTIO_BLK_T_IN, 1, 512).unwrap();
        assert_eq!(status(&m), 0);
        assert_eq!(m.read_obj::<u8>(GuestAddress(0x5000)).unwrap(), 0x5a);
        assert_eq!(m.read_obj::<u16>(GuestAddress(0x3002)).unwrap(), 2);
        assert!(b.interrupt_pending());
        b.write(0x64, 1, &m).unwrap();
        assert!(!b.interrupt_pending());
        submit(&mut b, &m, VIRTIO_BLK_T_FLUSH, 0, 0).unwrap();
        assert_eq!(d.flushes.get(), 1);
        b.write(0x70, 0, &m).unwrap();
        assert_eq!(b.status, 0);
        assert!(!b.queue.ready());
        assert_eq!(b.negotiated, 0);
        assert_eq!(b.queue.next_avail(), 0);
    }

    #[test]
    fn readonly_bounds_and_io_errors() {
        for (ro, fail, sector) in [
            (true, false, 0),
            (false, true, 0),
            (false, false, 8),
            (false, false, u64::MAX),
        ] {
            let (mut b, m, d) = setup(ro, fail);
            submit(&mut b, &m, VIRTIO_BLK_T_OUT, sector, 512).unwrap();
            assert_eq!(status(&m), 1);
            assert!(d.data.borrow().iter().all(|v| *v == 0));
        }
    }

    #[test]
    fn negotiation_rejects_unknown_and_missing_version() {
        let (mut b, m, _) = setup(false, false);
        b.write(0x70, 0, &m).unwrap();
        b.write(0x70, 3, &m).unwrap();
        b.write(0x20, 1, &m).unwrap();
        b.write(0x70, 11, &m).unwrap();
        assert_eq!(b.status & 8, 0);
        assert!(b.write(0x70, 15, &m).is_err());
    }

    #[test]
    fn malformed_chain_rejected() {
        let (mut b, m, _) = setup(false, false);
        m.write_obj(Descriptor::new(0x4000, 16, 1, 0), GuestAddress(0x1000))
            .unwrap();
        m.write_obj(1u16, GuestAddress(0x2002)).unwrap();
        assert!(b.write(0x50, 0, &m).is_err());
        let (mut b, m, _) = setup(false, false);
        m.write_obj(Descriptor::new(u64::MAX, 16, 1, 1), GuestAddress(0x1000))
            .unwrap();
        m.write_obj(Descriptor::new(0x6000, 1, 2, 0), GuestAddress(0x1010))
            .unwrap();
        m.write_obj(1u16, GuestAddress(0x2002)).unwrap();
        assert!(b.write(0x50, 0, &m).is_err());
    }

    #[test]
    fn get_id_unsupported_and_write_through() {
        let (mut b, m, d) = setup(false, false);
        submit(&mut b, &m, VIRTIO_BLK_T_GET_ID, 0, 20).unwrap();
        let mut id = [0; 20];
        m.read_slice(&mut id, GuestAddress(0x5000)).unwrap();
        assert_eq!(&id, b"w-vmm-data-000000001");
        assert_eq!(status(&m), 0);
        assert_eq!(m.read_obj::<u32>(GuestAddress(0x3008)).unwrap(), 21);
        submit(&mut b, &m, 0xffff, 0, 0).unwrap();
        assert_eq!(status(&m), 2);
        b.negotiated &= !(1 << VIRTIO_BLK_F_FLUSH);
        submit(&mut b, &m, VIRTIO_BLK_T_OUT, 0, 512).unwrap();
        assert_eq!(d.flushes.get(), 1);
    }

    #[test]
    fn rejects_indirect_and_avail_overrun() {
        let (mut b, m, _) = setup(false, false);
        m.write_obj(Descriptor::new(0x7000, 32, 4, 0), GuestAddress(0x1000))
            .unwrap();
        m.write_obj(1u16, GuestAddress(0x2002)).unwrap();
        assert!(
            b.write(0x50, 0, &m)
                .unwrap_err()
                .to_string()
                .contains("indirect")
        );
        let (mut b, m, _) = setup(false, false);
        m.write_obj(9u16, GuestAddress(0x2002)).unwrap();
        assert!(
            b.write(0x50, 0, &m)
                .unwrap_err()
                .to_string()
                .contains("overrun")
        );
    }
}
