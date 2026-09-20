use super::mmio::{Queues, VirtioDevice, read_config};
use crate::error::DeviceError;
use crate::storage::{BlockStorage, bounds};
use virtio_bindings::bindings::virtio_blk::*;
use virtio_queue::desc::split::Descriptor;
use vm_memory::{Bytes, GuestMemoryMmap};

type Result<T> = std::result::Result<T, DeviceError>;

const MAX_REQUEST: usize = 1024 * 1024;

pub(crate) struct Block<BS: BlockStorage> {
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
                .ok_or_else(|| DeviceError::MissingBlockChain)?;
            let used = self.request(mem, &chain.descriptors, queues.negotiated)?;
            queues.complete(0, mem, chain.head, used)?;
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        Ok(self.disk.flush()?)
    }

    fn flush(&self) -> Result<()> {
        Ok(self.disk.flush()?)
    }
}

impl<BS: BlockStorage> Block<BS> {
    pub(crate) fn new(name: String, disk: BS) -> Result<Self> {
        if !(!name.is_empty()
            && name.len() <= 20
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
        {
            return Err(DeviceError::InvalidBlockName { name });
        }
        let mut id = [0; 20];
        id[..name.len()].copy_from_slice(name.as_bytes());
        Ok(Self { disk, name, id })
    }

    fn request(&self, mem: &GuestMemoryMmap, desc: &[Descriptor], negotiated: u64) -> Result<u32> {
        if desc.len() < 2 {
            return Err(DeviceError::TruncatedBlockDescriptorChain);
        }
        let status = desc[desc.len() - 1];
        if !status.is_write_only() || status.len() != 1 {
            return Err(DeviceError::InvalidBlockHeaderStatus);
        }
        // Validate the entire request, including status, before any backend access.
        let all = super::buffers::slices(mem, desc)?;
        let request = &desc[..desc.len() - 1];
        let request_len = request
            .iter()
            .try_fold(0usize, |n, d| n.checked_add(d.len() as usize))
            .ok_or(DeviceError::RequestLengthOverflow)?;
        let total = request_len
            .checked_sub(16)
            .ok_or(DeviceError::InvalidBlockHeaderStatus)?;
        if total > MAX_REQUEST {
            return Err(DeviceError::BlockRequestTooLarge {
                length: total,
                limit: MAX_REQUEST,
            });
        }
        let mut remaining_header = 16;
        let mut data = Vec::new();
        for d in request {
            let header_len = remaining_header.min(d.len() as usize);
            if header_len != 0 && d.is_write_only() {
                return Err(DeviceError::InvalidBlockHeaderStatus);
            }
            remaining_header -= header_len;
            if d.len() as usize > header_len {
                data.push(*d);
            }
        }
        let mut h = [0; 16];
        super::buffers::copy_to(&all, &mut h);
        let kind = u32::from_le_bytes(h[..4].try_into()?);
        let sector = u64::from_le_bytes(h[8..].try_into()?);
        let buffers = super::buffers::range(&all, 16, total);
        let mut used = 1;
        let result = (|| -> Result<u8> {
            match kind {
                VIRTIO_BLK_T_IN | VIRTIO_BLK_T_OUT => {
                    let read = kind == VIRTIO_BLK_T_IN;
                    if !data.iter().all(|d| d.is_write_only() == read) {
                        return Err(DeviceError::BlockDescriptorDirectionMismatch);
                    }
                    if !total.is_multiple_of(512) {
                        return Err(DeviceError::UnalignedBlockRequest { length: total });
                    }
                    if !(read || !self.disk.read_only()) {
                        return Err(DeviceError::WriteToReadOnlyDisk);
                    }
                    let offset = sector
                        .checked_mul(512)
                        .ok_or_else(|| DeviceError::SectorOverflow { sector })?;
                    bounds(self.disk.size(), offset, total)?;
                    if read {
                        self.disk.read(offset, &buffers)?;
                        used += total as u32;
                    } else {
                        self.disk.write(offset, &buffers)?;
                        if negotiated & (1 << VIRTIO_BLK_F_FLUSH) == 0 {
                            self.disk.flush()?; // Without FLUSH negotiation, use write-through.
                        }
                    }
                }
                VIRTIO_BLK_T_FLUSH => {
                    if !(total == 0 && negotiated & (1 << VIRTIO_BLK_F_FLUSH) != 0) {
                        return Err(DeviceError::InvalidFlush);
                    }
                    self.disk.flush()?;
                }
                VIRTIO_BLK_T_GET_ID => {
                    if !(total >= 20 && data.iter().all(|d| d.is_write_only())) {
                        return Err(DeviceError::InvalidGetId);
                    }
                    super::buffers::copy_from(&buffers, &self.id);
                    used += 20;
                }
                _ => return Ok(VIRTIO_BLK_S_UNSUPP as u8),
            }
            Ok(VIRTIO_BLK_S_OK as u8)
        })();
        let status_value = match result {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "virtio-blk request {kind} ({}): {}",
                    self.name,
                    crate::error::diagnostic(&e)
                );
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
    use vm_memory::VolatileSlice;

    struct Fake {
        data: RefCell<Vec<u8>>,
        ro: bool,
        fail: bool,
        flushes: Cell<u32>,
        addresses: RefCell<Vec<usize>>,
        partial: Cell<bool>,
    }

    impl BlockStorage for Rc<Fake> {
        fn size(&self) -> u64 {
            self.data.borrow().len() as u64
        }

        fn read_only(&self) -> bool {
            self.ro
        }

        fn read(
            &self,
            o: u64,
            d: &[VolatileSlice<'_>],
        ) -> std::result::Result<(), crate::error::StorageError> {
            *self.addresses.borrow_mut() =
                d.iter().map(|s| s.ptr_guard().as_ptr() as usize).collect();
            if self.partial.get() {
                if let Some(first) = d.first() {
                    first.copy_from(&[0x99u8]);
                }
                return Err(crate::error::StorageError::Backend(
                    std::io::Error::other("partial read").into(),
                ));
            }
            if self.fail {
                return Err(crate::error::StorageError::Backend(
                    std::io::Error::other("injected host I/O error").into(),
                ));
            }
            let len = d.iter().map(VolatileSlice::len).sum();
            bounds(self.size(), o, len)?;
            super::super::buffers::copy_from(d, &self.data.borrow()[o as usize..o as usize + len]);
            Ok(())
        }

        fn write(
            &self,
            o: u64,
            d: &[VolatileSlice<'_>],
        ) -> std::result::Result<(), crate::error::StorageError> {
            *self.addresses.borrow_mut() =
                d.iter().map(|s| s.ptr_guard().as_ptr() as usize).collect();
            if !(!self.ro && !self.fail) {
                return Err(crate::error::StorageError::Backend(
                    std::io::Error::other("read-only/injected host I/O error").into(),
                ));
            }
            let len = d.iter().map(VolatileSlice::len).sum();
            bounds(self.size(), o, len)?;
            super::super::buffers::copy_to(
                d,
                &mut self.data.borrow_mut()[o as usize..o as usize + len],
            );
            Ok(())
        }

        fn flush(&self) -> std::result::Result<(), crate::error::StorageError> {
            self.flushes.set(self.flushes.get() + 1);
            if self.fail {
                return Err(crate::error::StorageError::Backend(
                    std::io::Error::other("injected sync error").into(),
                ));
            }
            Ok(())
        }
    }

    fn setup(ro: bool, fail: bool) -> (Mmio<Block<Rc<Fake>>>, GuestMemoryMmap, Rc<Fake>) {
        let disk = Rc::new(Fake {
            data: RefCell::new(vec![0; 4096]),
            ro,
            fail,
            flushes: Cell::new(0),
            addresses: RefCell::new(Vec::new()),
            partial: Cell::new(false),
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
    #[test]
    fn split_header_and_payload_cross_regions_reach_backend_directly() {
        use vm_memory::GuestMemoryBackend;
        let (_, _, disk) = setup(false, false);
        let b = Block::new("scatter".into(), disk.clone()).unwrap();
        let mem = GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0), 0x6000),
            (GuestAddress(0x6000), 0x2000),
        ])
        .unwrap();
        let desc = [
            Descriptor::new(0x4000, 7, 0, 0),
            Descriptor::new(0x4100, 9, 0, 0),
            Descriptor::new(0, 0, 0, 0),
            Descriptor::new(0x5f80, 512, 0, 0),
            Descriptor::new(0x7000, 1, 2, 0),
        ];
        mem.write_obj(VIRTIO_BLK_T_OUT, GuestAddress(0x4000))
            .unwrap();
        mem.write_slice(&[0x55; 512], GuestAddress(0x5f80)).unwrap();
        assert_eq!(b.request(&mem, &desc, 1 << VIRTIO_BLK_F_FLUSH).unwrap(), 1);
        let ptr = |addr, len| {
            mem.get_slice(GuestAddress(addr), len)
                .unwrap()
                .ptr_guard()
                .as_ptr() as usize
        };
        assert_eq!(
            *disk.addresses.borrow(),
            [ptr(0x5f80, 128), ptr(0x6000, 384)]
        );
        assert_eq!(&disk.data.borrow()[..512], &[0x55; 512]);
        let mut read_desc = desc;
        read_desc[3] = Descriptor::new(0x5f80, 512, 2, 0);
        mem.write_obj(VIRTIO_BLK_T_IN, GuestAddress(0x4000))
            .unwrap();
        assert_eq!(b.request(&mem, &read_desc, 0).unwrap(), 513);
        disk.partial.set(true);
        assert_eq!(b.request(&mem, &read_desc, 0).unwrap(), 1);
        assert_eq!(
            mem.read_obj::<u8>(GuestAddress(0x7000)).unwrap(),
            VIRTIO_BLK_S_IOERR as u8
        );
        assert_eq!(mem.read_obj::<u8>(GuestAddress(0x5f80)).unwrap(), 0x99);
        disk.addresses.borrow_mut().clear();
        read_desc[3] = Descriptor::new(0x7fff, 512, 2, 0);
        assert!(b.request(&mem, &read_desc, 0).is_err());
        assert!(disk.addresses.borrow().is_empty());
    }
}
