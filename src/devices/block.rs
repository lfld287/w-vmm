use super::mmio::{Queues, VirtioDevice, read_config};
use crate::storage::{BlockStorage, bounds};
use anyhow::{Result, ensure};
use virtio_bindings::bindings::virtio_blk::*;
use virtio_queue::desc::split::Descriptor;
use vm_memory::{Bytes, GuestMemoryMmap};

const MAX_REQUEST: usize = 1024 * 1024;

pub struct Block<BS: BlockStorage> {
    disk: BS,
    name: String,
    id: [u8; 20],
}

impl<BS: BlockStorage> VirtioDevice for Block<BS> {
    fn device_id(&self) -> u32 {
        2
    }
    fn features(&self) -> u64 {
        (1 << VIRTIO_BLK_F_FLUSH)
            | if self.disk.read_only() {
                1 << VIRTIO_BLK_F_RO
            } else {
                0
            }
    }
    fn queue_count(&self) -> usize {
        1
    }
    fn read_config(&self, offset: usize, data: &mut [u8]) {
        read_config(&(self.disk.size() / 512).to_le_bytes(), offset, data);
    }
    fn notify(&mut self, _queue: usize, queues: &mut Queues, mem: &GuestMemoryMmap) -> Result<()> {
        let count = queues.available(0, mem)?;
        for _ in 0..count {
            let chain = queues
                .pop(0, mem)?
                .ok_or_else(|| anyhow::anyhow!("missing block chain"))?;
            let used = self.request(mem, &chain.descriptors, queues.negotiated)?;
            queues.complete(0, mem, chain.head, used)?;
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        self.disk.flush()
    }
    fn flush(&self) -> Result<()> {
        self.disk.flush()
    }
}

impl<BS: BlockStorage> Block<BS> {
    pub fn new(name: String, disk: BS) -> Result<Self> {
        ensure!(
            !name.is_empty()
                && name.len() <= 20
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "block name must be 1..20 ASCII letters, digits, '.', '_' or '-'"
        );
        let mut id = [0; 20];
        id[..name.len()].copy_from_slice(name.as_bytes());
        Ok(Self { disk, name, id })
    }

    fn request(&self, mem: &GuestMemoryMmap, desc: &[Descriptor], negotiated: u64) -> Result<u32> {
        ensure!(desc.len() >= 2, "truncated block descriptor chain");
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
        let kind = u32::from_le_bytes(h[..4].try_into()?);
        let sector = u64::from_le_bytes(h[8..].try_into()?);
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
                        if negotiated & (1 << VIRTIO_BLK_F_FLUSH) == 0 {
                            self.disk.flush()?; // Without FLUSH negotiation, use write-through.
                        }
                    }
                }
                VIRTIO_BLK_T_FLUSH => {
                    ensure!(
                        total == 0 && negotiated & (1 << VIRTIO_BLK_F_FLUSH) != 0,
                        "invalid FLUSH"
                    );
                    self.disk.flush()?;
                }
                VIRTIO_BLK_T_GET_ID => {
                    ensure!(
                        total >= 20 && data.iter().all(|d| d.is_write_only()),
                        "invalid GET_ID"
                    );
                    let id = &self.id;
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
                eprintln!("virtio-blk request {kind} ({}): {e:#}", self.name);
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
    use crate::devices::mmio::Mmio;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use virtio_queue::QueueT;
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

    fn setup(ro: bool, fail: bool) -> (Mmio<Block<Rc<Fake>>>, GuestMemoryMmap, Rc<Fake>) {
        let disk = Rc::new(Fake {
            data: RefCell::new(vec![0; 4096]),
            ro,
            fail,
            flushes: Cell::new(0),
        });
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut b = Mmio::new(Block::new("w-vmm-data-000000001".into(), disk.clone()).unwrap());
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
        b: &mut Mmio<Block<Rc<Fake>>>,
        m: &GuestMemoryMmap,
        kind: u32,
        sector: u64,
        len: u32,
    ) -> Result<()> {
        let idx = b.queues.rings[0].next_avail();
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
        assert!(!b.queues.rings[0].ready());
        assert_eq!(b.queues.negotiated, 0);
        assert_eq!(b.queues.rings[0].next_avail(), 0);
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
        b.queues.negotiated &= !(1 << VIRTIO_BLK_F_FLUSH);
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

    #[test]
    fn named_disks_keep_io_status_and_interrupts_independent() {
        let (mut a, ma, da) = setup(false, false);
        let (mut b, mb, db) = setup(true, false);
        a.device = Block::new("sda".into(), da.clone()).unwrap();
        b.device = Block::new("sdb".into(), db.clone()).unwrap();
        ma.write_slice(&[0x33; 512], GuestAddress(0x5000)).unwrap();
        submit(&mut a, &ma, VIRTIO_BLK_T_OUT, 0, 512).unwrap();
        assert!(a.interrupt_pending());
        assert!(!b.interrupt_pending());
        assert_eq!(status(&ma), 0);
        assert_eq!(da.data.borrow()[0], 0x33);
        assert_eq!(db.data.borrow()[0], 0);
        submit(&mut b, &mb, VIRTIO_BLK_T_OUT, 0, 512).unwrap();
        assert_eq!(status(&mb), 1);
        a.write(0x70, 0, &ma).unwrap();
        assert!(b.interrupt_pending());
        submit(&mut b, &mb, VIRTIO_BLK_T_GET_ID, 0, 20).unwrap();
        let mut id = [0; 20];
        mb.read_slice(&mut id, GuestAddress(0x5000)).unwrap();
        assert_eq!(&id[..3], b"sdb");
        assert_eq!(&id[3..], &[0; 17]);
        for name in ["", "too-long-disk-name-123", "a/b", "磁盘"] {
            assert!(Block::new(name.into(), da.clone()).is_err());
        }
        let boxed: Box<dyn BlockStorage> = Box::new(da);
        assert!(Block::new("boxed".into(), boxed).is_ok());
    }
}
