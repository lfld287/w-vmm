use super::mmio::{MAX_QUEUE, Queues, VirtioDevice, read_config};
use crate::error::DeviceError;
use crate::storage::{BlockStorage, bounds};
use virtio_bindings::bindings::virtio_blk::*;
use virtio_queue::desc::split::Descriptor;
use vm_memory::{Bytes, GuestMemoryBackend, GuestMemoryMmap};

type Result<T> = std::result::Result<T, DeviceError>;

// Without INDIRECT_DESC, one chain also needs a header and a status descriptor.
const SEG_MAX: u32 = MAX_QUEUE as u32 - 2;

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
        // SIZE_MAX is deliberately absent: requests have no per-segment or aggregate size cap.
        (1 << VIRTIO_BLK_F_FLUSH)
            | (1 << VIRTIO_BLK_F_SEG_MAX)
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
        // capacity, size_max (ignored without SIZE_MAX), seg_max.
        let mut config = [0; 16];
        config[..8].copy_from_slice(&(self.disk.size() / 512).to_le_bytes());
        config[12..].copy_from_slice(&SEG_MAX.to_le_bytes());
        read_config(&config, offset, data);
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

    /// For a chain that passed queue validation, only an untrustworthy status
    /// descriptor is fatal. Any other malformed request is completed with IOERR
    /// and an unknown operation with UNSUPP.
    fn request(&self, mem: &GuestMemoryMmap, desc: &[Descriptor], negotiated: u64) -> Result<u32> {
        let (status, request) = desc
            .split_last()
            .ok_or(DeviceError::TruncatedBlockDescriptorChain)?;
        if !(status.is_write_only() && status.len() == 1 && mem.check_range(status.addr(), 1)) {
            return Err(DeviceError::InvalidBlockHeaderStatus);
        }
        let mut kind = None;
        let (status_value, used) = match self.execute(mem, request, negotiated, &mut kind) {
            Ok(done) => done,
            Err(e) => {
                let kind = kind.map_or_else(|| "?".into(), |k: u32| k.to_string());
                eprintln!(
                    "virtio-blk request {kind} ({}): {}",
                    self.name,
                    crate::error::diagnostic(&e)
                );
                (VIRTIO_BLK_S_IOERR as u8, 1)
            }
        };
        mem.write_obj(status_value, status.addr())?;
        Ok(used)
    }

    /// Returns the status byte and used length. Errors become IOERR.
    fn execute(
        &self,
        mem: &GuestMemoryMmap,
        request: &[Descriptor],
        negotiated: u64,
        kind: &mut Option<u32>,
    ) -> Result<(u8, u32)> {
        // Validate the entire request before any backend access.
        let all = super::buffers::slices(mem, request)?;
        let request_len = request
            .iter()
            .try_fold(0usize, |n, d| n.checked_add(d.len() as usize))
            .ok_or(DeviceError::RequestLengthOverflow)?;
        let total = request_len
            .checked_sub(16)
            .ok_or(DeviceError::InvalidBlockHeaderStatus)?;
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
        let k = u32::from_le_bytes(h[..4].try_into()?);
        *kind = Some(k);
        let sector = u64::from_le_bytes(h[8..].try_into()?);
        let buffers = super::buffers::range(&all, 16, total);
        match k {
            VIRTIO_BLK_T_IN | VIRTIO_BLK_T_OUT => {
                let read = k == VIRTIO_BLK_T_IN;
                if !data.iter().all(|d| d.is_write_only() == read) {
                    return Err(DeviceError::BlockDescriptorDirectionMismatch);
                }
                if !total.is_multiple_of(512) {
                    return Err(DeviceError::UnalignedBlockRequest { length: total });
                }
                if !(read || !self.disk.read_only()) {
                    return Err(DeviceError::WriteToReadOnlyDisk);
                }
                // The used length covers payload plus status and must fit before any I/O.
                let used = if read {
                    u32::try_from(total)
                        .ok()
                        .and_then(|n| n.checked_add(1))
                        .ok_or(DeviceError::RequestLengthOverflow)?
                } else {
                    1
                };
                let offset = sector
                    .checked_mul(512)
                    .ok_or_else(|| DeviceError::SectorOverflow { sector })?;
                bounds(self.disk.size(), offset, total)?;
                if read {
                    self.disk.read(offset, &buffers)?;
                } else {
                    self.disk.write(offset, &buffers)?;
                    if negotiated & (1 << VIRTIO_BLK_F_FLUSH) == 0 {
                        self.disk.flush()?; // Without FLUSH negotiation, use write-through.
                    }
                }
                Ok((VIRTIO_BLK_S_OK as u8, used))
            }
            VIRTIO_BLK_T_FLUSH => {
                if !(total == 0 && negotiated & (1 << VIRTIO_BLK_F_FLUSH) != 0) {
                    return Err(DeviceError::InvalidFlush);
                }
                self.disk.flush()?;
                Ok((VIRTIO_BLK_S_OK as u8, 1))
            }
            VIRTIO_BLK_T_GET_ID => {
                if !(total >= 20 && data.iter().all(|d| d.is_write_only())) {
                    return Err(DeviceError::InvalidGetId);
                }
                super::buffers::copy_from(&buffers, &self.id);
                Ok((VIRTIO_BLK_S_OK as u8, 21))
            }
            _ => Ok((VIRTIO_BLK_S_UNSUPP as u8, 1)),
        }
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
        capacity: Cell<Option<u64>>,
    }

    impl BlockStorage for Rc<Fake> {
        fn size(&self) -> u64 {
            self.capacity
                .get()
                .unwrap_or(self.data.borrow().len() as u64)
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
        setup_sized(ro, fail, 0x10000, 4096)
    }

    fn setup_sized(
        ro: bool,
        fail: bool,
        memory: usize,
        disk: usize,
    ) -> (Mmio<Block<Rc<Fake>>>, GuestMemoryMmap, Rc<Fake>) {
        let disk = Rc::new(Fake {
            data: RefCell::new(vec![0; disk]),
            ro,
            fail,
            flushes: Cell::new(0),
            addresses: RefCell::new(Vec::new()),
            partial: Cell::new(false),
            capacity: Cell::new(None),
        });
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), memory)]).unwrap();
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
        assert_eq!(b.request(&mem, &read_desc, 0).unwrap(), 1);
        assert_eq!(
            mem.read_obj::<u8>(GuestAddress(0x7000)).unwrap(),
            VIRTIO_BLK_S_IOERR as u8
        );
        assert!(disk.addresses.borrow().is_empty());
    }

    /// Write `chain` as descriptors 0.. and publish it on the 8-entry queue.
    fn submit_chain(
        b: &mut Mmio<Block<Rc<Fake>>>,
        m: &GuestMemoryMmap,
        chain: &[(u64, u32, bool)],
    ) -> Result<()> {
        let idx = b.queues.rings[0].next_avail();
        for (i, &(addr, len, write)) in chain.iter().enumerate() {
            let next = i + 1 < chain.len();
            let flags = u16::from(next) | if write { 2 } else { 0 };
            let d = Descriptor::new(addr, len, flags, if next { i as u16 + 1 } else { 0 });
            m.write_obj(d, GuestAddress(0x1000 + 16 * i as u64))?;
        }
        m.write_obj(0u16, GuestAddress(0x2004 + 2 * (idx % 8) as u64))?;
        m.write_obj(idx.wrapping_add(1).to_le(), GuestAddress(0x2002))?;
        b.write(0x50, 0, m)
    }

    fn header(m: &GuestMemoryMmap, kind: u32, sector: u64) {
        m.write_obj(kind.to_le(), GuestAddress(0x4000)).unwrap();
        m.write_obj(0u32, GuestAddress(0x4004)).unwrap();
        m.write_obj(sector.to_le(), GuestAddress(0x4008)).unwrap();
    }

    /// (used index, used length of the latest element)
    fn last_used(b: &Mmio<Block<Rc<Fake>>>, m: &GuestMemoryMmap) -> (u16, u32) {
        let idx: u16 = m.read_obj(GuestAddress(0x3002)).unwrap();
        let slot = u64::from(idx.wrapping_sub(1) % b.queues.rings[0].size());
        (
            idx,
            m.read_obj(GuestAddress(0x3004 + slot * 8 + 4)).unwrap(),
        )
    }

    #[test]
    fn advertises_seg_max_without_size_max() {
        let (b, _, _) = setup(false, false);
        let features = b.device.features();
        assert_ne!(features & (1 << VIRTIO_BLK_F_SEG_MAX), 0);
        assert_eq!(features & (1 << VIRTIO_BLK_F_SIZE_MAX), 0);
        assert_eq!(b.read(0x100, 8).unwrap(), 8); // 4096-byte disk in sectors.
        assert_eq!(b.read(0x108, 4).unwrap(), 0);
        assert_eq!(b.read(0x10c, 4).unwrap(), 126);
        assert_eq!(SEG_MAX as u16 + 2, MAX_QUEUE);
    }

    #[test]
    fn four_mib_multisegment_requests_complete() {
        const MIB4: usize = 4 << 20;
        let (mut b, m, d) = setup_sized(false, false, 0x80_0000 + MIB4, 2 * MIB4);
        let pattern: Vec<u8> = (0..MIB4).map(|i| (i / 512 % 251) as u8).collect();
        m.write_slice(&pattern, GuestAddress(0x80_0000)).unwrap();
        // Uneven segments that only add up to a sector multiple; max chain for queue size 8.
        let cuts = [0, 1000, 70_000, 1 << 20, 3 << 20, MIB4 - 333, MIB4];
        let segments = |write| {
            cuts.windows(2)
                .map(|w| (0x80_0000 + w[0] as u64, (w[1] - w[0]) as u32, write))
                .collect::<Vec<_>>()
        };
        let mut chain = vec![(0x4000, 16, false)];
        chain.extend(segments(false));
        chain.push((0x6000, 1, true));
        assert_eq!(chain.len(), 8);
        header(&m, VIRTIO_BLK_T_OUT, 8);
        submit_chain(&mut b, &m, &chain).unwrap();
        assert_eq!(status(&m), VIRTIO_BLK_S_OK as u8);
        assert_eq!(last_used(&b, &m), (1, 1));
        assert_eq!(&d.data.borrow()[4096..4096 + MIB4], &pattern[..]);

        m.write_slice(&vec![0; MIB4], GuestAddress(0x80_0000))
            .unwrap();
        let mut chain = vec![(0x4000, 16, false)];
        chain.extend(segments(true));
        chain.push((0x6000, 1, true));
        header(&m, VIRTIO_BLK_T_IN, 8);
        submit_chain(&mut b, &m, &chain).unwrap();
        assert_eq!(status(&m), VIRTIO_BLK_S_OK as u8);
        assert_eq!(last_used(&b, &m), (2, MIB4 as u32 + 1));
        let mut actual = vec![0; MIB4];
        m.read_slice(&mut actual, GuestAddress(0x80_0000)).unwrap();
        assert_eq!(actual, pattern);
        assert_eq!(d.addresses.borrow().len(), 6);

        // The advertised maximum: header, SEG_MAX data segments and status in one chain.
        let mut desc = vec![Descriptor::new(0x4000, 16, 0, 0)];
        let seg = (MIB4 / SEG_MAX as usize) as u32 & !511;
        for i in 0..SEG_MAX as u64 {
            desc.push(Descriptor::new(0x80_0000 + i * u64::from(seg), seg, 2, 0));
        }
        desc.push(Descriptor::new(0x6000, 1, 2, 0));
        assert_eq!(desc.len(), MAX_QUEUE as usize);
        let used = b
            .device
            .request(&m, &desc, 1 << VIRTIO_BLK_F_FLUSH)
            .unwrap();
        assert_eq!(used, SEG_MAX * seg + 1);
        assert_eq!(status(&m), VIRTIO_BLK_S_OK as u8);
        assert_eq!(d.addresses.borrow().len(), SEG_MAX as usize);
    }

    #[test]
    fn malformed_requests_complete_with_ioerr_and_unknown_with_unsupp() {
        let ioerr = VIRTIO_BLK_S_IOERR as u8;
        let header_ro = (0x4000, 16, false);
        let status_wo = (0x6000, 1, true);
        let cases = [
            // Header shorter than 16 bytes.
            (
                VIRTIO_BLK_T_IN,
                0,
                vec![(0x4000, 8, false), status_wo],
                ioerr,
            ),
            // Only the status descriptor.
            (VIRTIO_BLK_T_IN, 0, vec![status_wo], ioerr),
            // Device-writable header.
            (
                VIRTIO_BLK_T_IN,
                0,
                vec![(0x4000, 16, true), (0x5000, 512, true), status_wo],
                ioerr,
            ),
            // Sector whose byte offset overflows.
            (
                VIRTIO_BLK_T_OUT,
                u64::MAX,
                vec![header_ro, (0x5000, 512, false), status_wo],
                ioerr,
            ),
            // Read into a device-readable buffer.
            (
                VIRTIO_BLK_T_IN,
                0,
                vec![header_ro, (0x5000, 512, false), status_wo],
                ioerr,
            ),
            // Payload is not a sector multiple.
            (
                VIRTIO_BLK_T_OUT,
                0,
                vec![header_ro, (0x5000, 100, false), status_wo],
                ioerr,
            ),
            // Sector beyond the disk.
            (
                VIRTIO_BLK_T_OUT,
                8,
                vec![header_ro, (0x5000, 512, false), status_wo],
                ioerr,
            ),
            // FLUSH carrying data.
            (
                VIRTIO_BLK_T_FLUSH,
                0,
                vec![header_ro, (0x5000, 512, false), status_wo],
                ioerr,
            ),
            // GET_ID buffer too short.
            (
                VIRTIO_BLK_T_GET_ID,
                0,
                vec![header_ro, (0x5000, 8, true), status_wo],
                ioerr,
            ),
            // Unknown operation, even with an otherwise invalid payload.
            (
                0x7777,
                0,
                vec![header_ro, (0x5000, 3, true), status_wo],
                VIRTIO_BLK_S_UNSUPP as u8,
            ),
        ];
        let (mut b, m, d) = setup(false, false);
        for (i, (kind, sector, chain, expected)) in cases.into_iter().enumerate() {
            header(&m, kind, sector);
            m.write_obj(0xffu8, GuestAddress(0x6000)).unwrap();
            b.write(0x64, 1, &m).unwrap();
            submit_chain(&mut b, &m, &chain).unwrap_or_else(|e| panic!("case {i}: {e}"));
            assert_eq!(status(&m), expected, "case {i}");
            assert_eq!(last_used(&b, &m), (i as u16 + 1, 1), "case {i}");
            assert!(b.interrupt_pending(), "case {i}");
        }
        assert!(d.addresses.borrow().is_empty());
        assert_eq!(d.flushes.get(), 0);
        assert!(d.data.borrow().iter().all(|v| *v == 0));
        // The queue keeps working after malformed requests.
        m.write_slice(&[0x42; 512], GuestAddress(0x5000)).unwrap();
        submit(&mut b, &m, VIRTIO_BLK_T_OUT, 0, 512).unwrap();
        assert_eq!(status(&m), VIRTIO_BLK_S_OK as u8);
        assert_eq!(d.data.borrow()[0], 0x42);
    }

    #[test]
    fn lenient_keeps_request_status_and_isolates_only_untrusted_chains() {
        let ioerr = VIRTIO_BLK_S_IOERR as u8;
        let (b, m, d) = setup(false, false);
        let mut b = b.with_strict(false);
        // Malformed requests and backend errors behind a trusted status
        // descriptor are normal responses in either mode.
        header(&m, VIRTIO_BLK_T_OUT, 0);
        let chain = [(0x4000, 16, false), (0x5000, 100, false), (0x6000, 1, true)];
        submit_chain(&mut b, &m, &chain).unwrap();
        assert_eq!((status(&m), b.needs_reset()), (ioerr, false));
        // An untrustworthy status descriptor isolates the device.
        b.write(0x64, 1, &m).unwrap();
        submit_chain(&mut b, &m, &[(0x4000, 16, false), (0x6000, 2, true)]).unwrap();
        assert!(b.needs_reset());
        assert_eq!(b.read(0x60, 4), Some(2));
        assert_eq!(last_used(&b, &m), (1, 1));
        assert!(d.addresses.borrow().is_empty());
        // Host failures stay fatal: reset flushes the disk.
        let (b, m, _) = setup(false, true);
        let mut b = b.with_strict(false);
        assert!(matches!(b.write(0x70, 0, &m), Err(DeviceError::Storage(_))));
        assert_eq!(b.status, 15);
    }

    #[test]
    fn read_used_length_overflow_rejected_before_backend() {
        const SEG: u32 = 1 << 28;
        let (_, m, d) = setup_sized(false, false, 0x10000 + SEG as usize, 4096);
        let b = Block::new("overflow".into(), d.clone()).unwrap();
        // 16 aliasing segments add up to exactly 4 GiB, so used = 4 GiB + 1 overflows u32.
        let mut desc = vec![Descriptor::new(0x4000, 16, 0, 0)];
        desc.extend((0..16).map(|_| Descriptor::new(0x10000, SEG, 2, 0)));
        desc.push(Descriptor::new(0x6000, 1, 2, 0));
        d.capacity.set(Some(1 << 40)); // Bounds checks alone would accept the request.
        header(&m, VIRTIO_BLK_T_IN, 0);
        assert_eq!(b.request(&m, &desc, 0).unwrap(), 1);
        assert_eq!(status(&m), VIRTIO_BLK_S_IOERR as u8);
        assert!(d.addresses.borrow().is_empty());
        // One sector less fits (used = 4 GiB - 511) and reaches the backend, which fails here.
        desc[16] = Descriptor::new(0x10000, SEG - 512, 2, 0);
        d.partial.set(true);
        assert_eq!(b.request(&m, &desc, 0).unwrap(), 1);
        assert_eq!(d.addresses.borrow().len(), 16);
    }
}
