//! virtio-mem 1.2 and transactional, sparse guest-memory mappings.
use super::mmio::{self, Queues, VirtioDevice};
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, sync::Arc};
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemoryMmap, GuestMemoryRegion, GuestRegionMmap,
};

// virtio-mem protocol/mapping granularity, independent of Linux hotplug blocks.
const BLOCK: u64 = 2 << 20;
// Linux memory hotplug block size for the bundled guest kernel.
const HOTPLUG_BLOCK_SIZE: u64 = 128 << 20;

#[cfg(test)]
use crate::memory::MemoryStatus;
use crate::memory::{Mapper, MemoryControl, MemoryLifecycle};

/// Sparse virtio-mem device. Initial requested and plugged capacity are zero.
pub(crate) struct VirtioMem {
    control: MemoryControl,
    pub(crate) start: u64,
    capacity: u64,
    requested: u64,
    plugged: u64,
    generation: u32,
    blocks: BTreeMap<u64, Arc<GuestRegionMmap>>,
    // A failed rollback may leave mappings alive until the platform VM is destroyed.
    retained: Vec<Arc<GuestRegionMmap>>,
}

impl VirtioMem {
    fn plugged(&self, addr: u64) -> bool {
        self.blocks.contains_key(&addr)
    }

    fn count(&self) -> u64 {
        self.blocks.len() as u64
    }

    fn addresses(&self) -> Vec<u64> {
        self.blocks.keys().copied().collect()
    }

    /// Called only with every vCPU quiescent and device processing suspended.
    /// false means failure with successful rollback; Err is fatal rollback failure.
    fn change<M: Mapper>(
        &mut self,
        addresses: &[u64],
        plug: bool,
        mapper: &mut M,
        view: &mut GuestMemoryMmap,
    ) -> Result<bool> {
        let mut regions = Vec::new();
        let mut next = view.clone();
        for &addr in addresses {
            let region = if plug {
                Arc::new(GuestRegionMmap::from_range(
                    GuestAddress(addr),
                    BLOCK as usize,
                    None,
                )?)
            } else {
                self.blocks[&addr].clone()
            };
            next = if plug {
                next.insert_region(region.clone())?
            } else {
                next.remove_region(GuestAddress(addr), BLOCK)?.0
            };
            regions.push(region);
        }
        for (done, region) in regions.iter().enumerate() {
            let result = if plug {
                mapper.map(region)
            } else {
                mapper.unmap(region)
            };
            if result.is_err() {
                let mut failed = false;
                for r in regions[..done].iter().rev() {
                    failed |= if plug { mapper.unmap(r) } else { mapper.map(r) }.is_err();
                }
                if failed {
                    self.retained.extend(regions);
                    anyhow::bail!("memory mapping rollback failed; stopping VM");
                }
                return Ok(false);
            }
        }
        *view = next;
        for (&addr, region) in addresses.iter().zip(regions) {
            if plug {
                self.blocks.insert(addr, region);
            } else {
                self.blocks.remove(&addr);
            }
        }
        Ok(true)
    }
}

impl VirtioMem {
    pub(crate) fn new(region_size_mib: u64) -> Result<Self> {
        ensure!(
            region_size_mib > 0
                && region_size_mib.is_multiple_of(HOTPLUG_BLOCK_SIZE / crate::memory::MIB),
            "virtio-mem region must be a positive multiple of {} MiB",
            HOTPLUG_BLOCK_SIZE / crate::memory::MIB
        );
        let capacity = crate::memory::mib_bytes(region_size_mib)?;
        Ok(Self {
            start: 0,
            capacity,
            requested: 0,
            plugged: 0,
            generation: 0,
            control: MemoryControl::new(region_size_mib),
            blocks: BTreeMap::new(),
            retained: Vec::new(),
        })
    }

    pub(crate) fn control(&self) -> MemoryControl {
        self.control.clone()
    }

    pub(crate) fn attach_at(&mut self, start: u64) -> Result<()> {
        ensure!(
            start.is_multiple_of(HOTPLUG_BLOCK_SIZE),
            "hotplug alignment"
        );
        start
            .checked_add(self.capacity)
            .ok_or_else(|| anyhow::anyhow!("hotplug overflow"))?;
        self.start = start;
        self.requested = crate::memory::mib_bytes(self.control.status().requested_size_mib)?;
        Ok(())
    }

    #[cfg(test)]
    fn attach(&mut self, base_mib: u64) -> Result<()> {
        let end = 0x4000_0000u64
            .checked_add(crate::memory::mib_bytes(base_mib)?)
            .and_then(|v| v.checked_next_multiple_of(HOTPLUG_BLOCK_SIZE))
            .ok_or_else(|| anyhow::anyhow!("hotplug overflow"))?;
        self.attach_at(end)
    }

    #[cfg(test)]
    fn validate_ipa(&self, bits: u32) -> Result<()> {
        ensure!(
            self.start
                .checked_add(self.capacity)
                .is_some_and(|e| bits == 64 || e <= 1u64 << bits),
            "IPA overflow"
        );
        Ok(())
    }

    pub(crate) fn start(&self) {
        self.control.lock().lifecycle = MemoryLifecycle::Running;
    }

    /// vCPUs must be joined first. Keep allocations until after VM destruction,
    /// including those whose mapping state became uncertain during rollback.
    pub(crate) fn stop<M: Mapper>(&self, mapper: &mut M) {
        let mut status = self.control.lock();
        status.lifecycle = MemoryLifecycle::Stopped;
        status.driver_ready = false;
        let mut regions = BTreeMap::new();
        for region in self.blocks.values().chain(&self.retained) {
            regions.insert(region.start_addr().raw_value(), region);
        }
        for region in regions.values() {
            if let Err(e) = mapper.unmap(region) {
                eprintln!("final hotplug unmap: {e:#}");
            }
        }
    }

    pub(crate) fn sync_target(&mut self, ready: bool) -> bool {
        let mut status = self.control.lock();
        status.driver_ready = ready;
        // Accepted targets are bounded by the byte-checked region capacity.
        let requested = status.requested_size_mib * crate::memory::MIB;
        if requested == self.requested {
            return false;
        }
        self.requested = requested;
        self.generation = self.generation.wrapping_add(1);
        true
    }

    fn request<M: Mapper>(
        &mut self,
        req: &[u8; 24],
        mapper: &mut M,
        view: &mut GuestMemoryMmap,
        pinned: &[(u64, u64)],
    ) -> Result<(u16, u16)> {
        let kind = u16::from_le_bytes(req[..2].try_into()?);
        let addr = u64::from_le_bytes(req[8..16].try_into()?);
        let count = u16::from_le_bytes(req[16..18].try_into()?) as u64;
        if kind > 3 {
            return Ok((3, 0));
        }
        let addresses = if kind == 2 {
            self.addresses()
        } else {
            let Some(end) = addr.checked_add(count * BLOCK) else {
                return Ok((3, 0));
            };
            if count == 0
                || !addr.is_multiple_of(BLOCK)
                || addr < self.start
                || end > self.start + self.capacity
            {
                return Ok((3, 0));
            }
            (0..count).map(|n| addr + n * BLOCK).collect::<Vec<_>>()
        };
        let plugged = addresses.iter().filter(|&&a| self.plugged(a)).count();
        if kind == 3 {
            return Ok((
                0,
                if plugged == 0 {
                    1
                } else if plugged == addresses.len() {
                    0
                } else {
                    2
                },
            ));
        }
        if (kind == 0 && plugged != 0) || (kind == 1 && plugged != addresses.len()) {
            return Ok((3, 0));
        }
        // Serialize target acceptance with the transaction and plugged-size update.
        let control = self.control.clone();
        let mut status = control.lock();
        if kind == 0
            && (self.count() + count) > status.requested_size_mib / (BLOCK / crate::memory::MIB)
        {
            return Ok((1, 0));
        }
        if kind != 0
            && addresses.iter().any(|a| {
                pinned
                    .iter()
                    .any(|&(p, len)| p < a + BLOCK && p.saturating_add(len) > *a)
            })
        {
            return Ok((2, 0));
        }
        if !self.change(&addresses, kind == 0, mapper, view)? {
            return Ok((2, 0));
        }
        self.plugged = self.count() * BLOCK;
        self.generation = self.generation.wrapping_add(1);
        status.plugged_size_mib = self.plugged >> 20;
        Ok((0, 0))
    }

    pub(crate) fn process<M: Mapper>(
        &mut self,
        queues: &mut Queues,
        mapper: &mut M,
        view: &mut GuestMemoryMmap,
        pinned: &[(u64, u64)],
    ) -> Result<()> {
        // Bound work so host stop/input requests remain responsive.
        for _ in 0..128 {
            let Some(chain) = queues.pop(0, view)? else {
                break;
            };
            let read: usize = chain
                .descriptors
                .iter()
                .filter(|d| !d.is_write_only())
                .map(|d| d.len() as usize)
                .sum();
            let write: usize = chain
                .descriptors
                .iter()
                .filter(|d| d.is_write_only())
                .map(|d| d.len() as usize)
                .sum();
            ensure!(
                read == 24 && write >= 10,
                "invalid virtio-mem request buffers"
            );
            let mut req = [0; 24];
            let mut offset = 0;
            let mut writable = false;
            let mut pins = pinned.to_vec();
            for d in &chain.descriptors {
                pins.push((d.addr().raw_value(), d.len() as u64));
                if d.is_write_only() {
                    writable = true;
                } else {
                    ensure!(!writable, "readable descriptor after writable descriptor");
                    view.read_slice(&mut req[offset..offset + d.len() as usize], d.addr())?;
                    offset += d.len() as usize;
                }
            }
            let (code, state) = self.request(&req, mapper, view, &pins)?;
            let mut response = [0; 10];
            response[..2].copy_from_slice(&code.to_le_bytes());
            response[8..].copy_from_slice(&state.to_le_bytes());
            let mut offset = 0;
            for d in chain.descriptors.iter().filter(|d| d.is_write_only()) {
                let len = (d.len() as usize).min(10 - offset);
                view.write_slice(&response[offset..offset + len], d.addr())?;
                offset += len;
                if offset == 10 {
                    break;
                }
            }
            queues.complete(0, view, chain.head, 10)?;
        }
        Ok(())
    }
}

impl VirtioDevice for VirtioMem {
    fn required_features(&self) -> u64 {
        2
    }

    fn generation(&self) -> u32 {
        self.generation
    }

    fn device_id(&self) -> u32 {
        24
    }

    fn features(&self) -> u64 {
        2
    }

    fn queue_count(&self) -> usize {
        1
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        let mut config = [0; 56];
        for (offset, value) in [
            (0, BLOCK),
            (16, self.start),
            (24, self.capacity),
            (32, self.capacity),
            (40, self.plugged),
            (48, self.requested),
        ] {
            config[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        mmio::read_config(&config, offset, data);
    }

    fn notify(&mut self, _: usize, _: &mut Queues, _: &GuestMemoryMmap) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.control.lock().driver_ready = false;
        Ok(())
    }
}

impl Drop for VirtioMem {
    fn drop(&mut self) {
        let mut status = self.control.lock();
        status.lifecycle = MemoryLifecycle::Stopped;
        status.driver_ready = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::mmio::{Mmio, tests::initialize};
    use std::collections::BTreeSet;
    use vm_memory::{GuestMemoryBackend, GuestMemoryRegion};

    #[derive(Default)]
    struct Fake {
        mapped: BTreeSet<u64>,
        calls: usize,
        fail: Vec<usize>,
    }

    impl Mapper for Fake {
        fn map(&mut self, r: &GuestRegionMmap) -> Result<()> {
            self.calls += 1;
            ensure!(!self.fail.contains(&self.calls), "injected map failure");
            assert!(self.mapped.insert(r.start_addr().0));
            Ok(())
        }

        fn unmap(&mut self, r: &GuestRegionMmap) -> Result<()> {
            self.calls += 1;
            ensure!(!self.fail.contains(&self.calls), "injected unmap failure");
            assert!(self.mapped.remove(&r.start_addr().0));
            Ok(())
        }
    }

    #[test]
    fn capacity_and_lifecycle() {
        const H: u64 = HOTPLUG_BLOCK_SIZE / crate::memory::MIB;
        for size in [0, 1, H + 1, u64::MAX, (u64::MAX / H) * H] {
            assert!(VirtioMem::new(size).is_err());
        }
        let mut max = VirtioMem::new(16384).unwrap();
        max.attach(32768).unwrap();
        max.validate_ipa(36).unwrap();
        let mut d = VirtioMem::new(HOTPLUG_BLOCK_SIZE / crate::memory::MIB).unwrap();
        let c = d.control();
        assert_eq!(
            c.status(),
            MemoryStatus {
                region_size_mib: H,
                requested_size_mib: 0,
                plugged_size_mib: 0,
                driver_ready: false,
                lifecycle: MemoryLifecycle::Created,
            }
        );
        assert!(c.set_requested_mib(3).is_err());
        assert!(c.set_requested_mib(2 * H).is_err());
        std::thread::scope(|s| {
            for n in 0..32 {
                let c = c.clone();
                s.spawn(move || c.set_requested_mib((n % 2) * H).unwrap());
            }
        });
        c.set_requested_mib(H).unwrap();
        assert!(d.attach(u64::MAX).is_err());
        d.attach(128 * H + 1).unwrap();
        assert_eq!(d.start, 0x4000_0000u64 + 129 * HOTPLUG_BLOCK_SIZE);
        assert_eq!(d.start % HOTPLUG_BLOCK_SIZE, 0);
        assert_eq!(d.requested, HOTPLUG_BLOCK_SIZE);
        d.start();
        assert_eq!(c.status().lifecycle, MemoryLifecycle::Running);
        d.stop(&mut Fake::default());
        assert_eq!(c.status().lifecycle, MemoryLifecycle::Stopped);
        assert!(c.set_requested_mib(0).is_err());
        let unused = VirtioMem::new(HOTPLUG_BLOCK_SIZE / crate::memory::MIB).unwrap();
        let c = unused.control();
        drop(unused);
        assert_eq!(c.status().lifecycle, MemoryLifecycle::Stopped);
    }

    #[test]
    fn region_address_boundaries() {
        const H: u64 = HOTPLUG_BLOCK_SIZE / crate::memory::MIB;
        let mut d = VirtioMem::new(2 * H).unwrap();
        for target in [0, H, 2 * H] {
            d.control().set_requested_mib(target).unwrap();
        }
        for target in [1, H - 1, H + 1, 3 * H, u64::MAX] {
            assert!(d.control().set_requested_mib(target).is_err());
            assert_eq!(d.control().status().requested_size_mib, 2 * H);
        }
        let limit = 1u64 << 36;
        let base_mib = (limit - 0x4000_0000u64 - d.capacity) / crate::memory::MIB;
        d.attach(base_mib).unwrap();
        d.validate_ipa(36).unwrap();
        d.attach(base_mib + 1).unwrap();
        assert!(d.validate_ipa(36).is_err()); // Include the alignment gap.
        assert!(d.attach(u64::MAX / crate::memory::MIB).is_err()); // Base end.
        assert!(
            d.attach((u64::MAX - 0x4000_0000u64) / crate::memory::MIB)
                .is_err()
        ); // Alignment.
        let capacity = (u64::MAX / HOTPLUG_BLOCK_SIZE) * H;
        let mut huge = VirtioMem::new(capacity).unwrap();
        assert!(huge.attach(H).is_err()); // Region end.
    }

    #[test]
    fn large_sparse_region_grows_and_reclaims_allocations() {
        const H: u64 = HOTPLUG_BLOCK_SIZE / crate::memory::MIB;
        let mut d = VirtioMem::new(32768).unwrap();
        d.attach(512).unwrap();
        let mut view = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut mapper = Fake::default();
        let mut allocations = Vec::new();
        for target in [0, H, 2 * H, H, 0] {
            d.control().set_requested_mib(target).unwrap();
            let blocks = target * crate::memory::MIB / BLOCK;
            let old = d.count();
            if blocks != old {
                let (kind, first, count) = if blocks > old {
                    (0, old, blocks - old)
                } else {
                    (1, blocks, old - blocks)
                };
                assert_eq!(
                    d.request(
                        &req(kind, d.start + first * BLOCK, count.try_into().unwrap()),
                        &mut mapper,
                        &mut view,
                        &[]
                    )
                    .unwrap()
                    .0,
                    0
                );
                allocations.extend(d.blocks.values().map(Arc::downgrade));
            }
            assert_eq!(d.count(), blocks);
            assert_eq!(mapper.mapped.len() as u64, blocks);
            assert_eq!(view.num_regions() as u64, blocks + 1);
            assert_eq!(d.control().status().plugged_size_mib, target);
        }
        assert!(allocations.iter().all(|r| r.upgrade().is_none()));
    }

    #[test]
    fn rollback_allocations_live_through_vm_teardown() {
        let (mut d, mut view, mut mapper) = setup();
        mapper.fail = vec![2, 3];
        let a = d.start;
        assert!(
            d.request(&req(0, a, 2), &mut mapper, &mut view, &[])
                .is_err()
        );
        assert_eq!(d.count(), 0);
        assert_eq!(d.control().status().plugged_size_mib, 0);
        assert!(!view.check_range(GuestAddress(a), 1));
        let allocations: Vec<_> = d.retained.iter().map(Arc::downgrade).collect();
        // Simulate failed final unmaps, then destruction of the platform mapping state.
        mapper.fail.extend([4, 5]);
        d.stop(&mut mapper);
        assert!(allocations.iter().all(|r| r.upgrade().is_some()));
        drop(mapper);
        drop(view);
        assert!(allocations.iter().all(|r| r.upgrade().is_some()));
        drop(d);
        assert!(allocations.iter().all(|r| r.upgrade().is_none()));
    }

    fn setup() -> (VirtioMem, GuestMemoryMmap, Fake) {
        let mut d = VirtioMem::new(HOTPLUG_BLOCK_SIZE / crate::memory::MIB).unwrap();
        d.control()
            .set_requested_mib(HOTPLUG_BLOCK_SIZE / crate::memory::MIB)
            .unwrap();
        d.attach(512).unwrap();
        (
            d,
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap(),
            Fake::default(),
        )
    }

    fn req(kind: u16, addr: u64, count: u16) -> [u8; 24] {
        let mut bytes = [0; 24];
        bytes[..2].copy_from_slice(&kind.to_le_bytes());
        bytes[8..16].copy_from_slice(&addr.to_le_bytes());
        bytes[16..18].copy_from_slice(&count.to_le_bytes());
        bytes
    }

    #[test]
    fn requests_state_target_reset_and_holes() {
        let (mut d, mut view, mut mapper) = setup();
        let a = d.start;
        assert_eq!(
            d.request(&req(3, a, 2), &mut mapper, &mut view, &[])
                .unwrap(),
            (0, 1)
        );
        assert_eq!(
            d.request(&req(0, a, 1), &mut mapper, &mut view, &[])
                .unwrap()
                .0,
            0
        );
        assert_eq!(
            d.request(&req(0, a, 1), &mut mapper, &mut view, &[])
                .unwrap()
                .0,
            3
        );
        assert_eq!(
            d.request(&req(3, a, 2), &mut mapper, &mut view, &[])
                .unwrap(),
            (0, 2)
        );
        assert!(!view.check_range(GuestAddress(a + BLOCK - 8), 16));
        assert_eq!(
            d.request(&req(0, a + BLOCK, 1), &mut mapper, &mut view, &[])
                .unwrap()
                .0,
            0
        );
        assert!(view.check_range(GuestAddress(a + BLOCK - 8), 16));
        view.write_slice(&[42; 16], GuestAddress(a + BLOCK - 8))
            .unwrap();
        d.reset().unwrap();
        let mut data = [0; 16];
        view.read_slice(&mut data, GuestAddress(a + BLOCK - 8))
            .unwrap();
        assert_eq!(data, [42; 16]);
        assert_eq!(
            d.request(&req(3, a, 2), &mut mapper, &mut view, &[])
                .unwrap(),
            (0, 0)
        );
        assert_eq!(
            d.request(&req(1, a, 1), &mut mapper, &mut view, &[(a + 16, 4)])
                .unwrap()
                .0,
            2
        );
        assert_eq!(
            d.request(&req(1, a, 1), &mut mapper, &mut view, &[])
                .unwrap()
                .0,
            0
        );
        assert_eq!(
            d.request(&req(1, a, 2), &mut mapper, &mut view, &[])
                .unwrap()
                .0,
            3
        );
        d.control.set_requested_mib(0).unwrap();
        assert_eq!(
            d.request(&req(0, a, 1), &mut mapper, &mut view, &[])
                .unwrap()
                .0,
            1
        );
        assert_eq!(
            d.request(&req(2, u64::MAX, 0), &mut mapper, &mut view, &[])
                .unwrap()
                .0,
            0
        );
        assert!(mapper.mapped.is_empty());
        assert_eq!(d.control.status().plugged_size_mib, 0);
    }

    #[test]
    fn invalid_requests() {
        let (mut d, mut view, mut mapper) = setup();
        let a = d.start;
        for (kind, addr, n) in [
            (0, a, 0),
            (0, a + 1, 1),
            (0, a - BLOCK, 1),
            (0, a + HOTPLUG_BLOCK_SIZE, 1),
            (0, u64::MAX, 2),
            (7, a, 1),
        ] {
            assert_eq!(
                d.request(&req(kind, addr, n), &mut mapper, &mut view, &[])
                    .unwrap()
                    .0,
                3
            );
        }
    }

    #[test]
    fn transactional_map_and_unmap_failure() {
        let (mut d, mut view, mut mapper) = setup();
        let a = d.start;
        mapper.fail = vec![2];
        assert_eq!(
            d.request(&req(0, a, 2), &mut mapper, &mut view, &[])
                .unwrap()
                .0,
            2
        );
        assert_eq!(d.count(), 0);
        assert!(mapper.mapped.is_empty());
        mapper.fail.clear();
        d.request(&req(0, a, 2), &mut mapper, &mut view, &[])
            .unwrap();
        view.write_slice(&[91], GuestAddress(a)).unwrap();
        mapper.fail = vec![mapper.calls + 2];
        assert_eq!(
            d.request(&req(1, a, 2), &mut mapper, &mut view, &[])
                .unwrap()
                .0,
            2
        );
        assert_eq!(d.count(), 2);
        assert_eq!(mapper.mapped.len(), 2);
        assert_eq!(view.read_obj::<u8>(GuestAddress(a)).unwrap(), 91);
        mapper.fail = vec![mapper.calls + 2, mapper.calls + 3];
        assert!(
            d.request(&req(1, a, 2), &mut mapper, &mut view, &[])
                .is_err()
        );
        assert_eq!(d.retained.len(), 2);
    }

    #[test]
    fn negotiation_generation_and_queue_response() {
        let (mut d, mut view, mut mapper) = setup();
        d.control().set_requested_mib(0).unwrap();
        d.attach(512).unwrap();
        let a = d.start;
        let mut mmio = Mmio::new(d);
        mmio.write(0x24, 1, &view).unwrap();
        mmio.write(0x20, 1, &view).unwrap();
        mmio.write(0x70, 11, &view).unwrap();
        assert_eq!(mmio.read(0x70, 4) & 8, 0);
        mmio.write(0x70, 0, &view).unwrap();
        initialize(&mut mmio, &view);
        mmio.device
            .control()
            .set_requested_mib(HOTPLUG_BLOCK_SIZE / crate::memory::MIB)
            .unwrap();
        assert!(mmio.device.sync_target(true));
        mmio.config_changed();
        assert_eq!(mmio.read(0xfc, 4), 1);
        assert_eq!(mmio.read(0x60, 4), 2);
        assert!(mmio.device.control.status().driver_ready);
        use virtio_queue::desc::split::Descriptor;
        view.write_obj(Descriptor::new(0x8000, 24, 1, 1), GuestAddress(0x1000))
            .unwrap();
        view.write_obj(Descriptor::new(0x9000, 10, 2, 0), GuestAddress(0x1010))
            .unwrap();
        view.write_slice(&req(0, a, 2), GuestAddress(0x8000))
            .unwrap();
        view.write_obj(1u16, GuestAddress(0x2002)).unwrap();
        let pins = mmio.queues.pinned(&view).unwrap();
        mmio.device
            .process(&mut mmio.queues, &mut mapper, &mut view, &pins)
            .unwrap();
        assert_eq!(view.read_obj::<u16>(GuestAddress(0x9000)).unwrap(), 0);
        assert_eq!(mmio.device.count(), 2);
        assert_eq!(mmio.read(0x128, 8), 2 * BLOCK);
        mmio.write(0x70, 0, &view).unwrap();
        assert_eq!(mmio.device.count(), 2);
        assert!(!mmio.device.control.status().driver_ready);
    }
}
