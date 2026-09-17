//! virtio-mem 1.2 and transactional, sparse guest-memory mappings.
use super::mmio::{self, Queues, VirtioDevice};
use crate::{
    MemoryControl,
    memory::{ALIGN, BLOCK},
};
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, sync::Arc};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryMmap, GuestRegionMmap};

pub trait Mapper {
    fn map(&mut self, region: &GuestRegionMmap) -> Result<()>;
    fn unmap(&mut self, region: &GuestRegionMmap) -> Result<()>;
}
pub struct Memory<M: Mapper> {
    pub view: GuestMemoryMmap,
    pub mapper: M,
    blocks: BTreeMap<u64, Arc<GuestRegionMmap>>,
    // Retain host allocations if rollback fails; VM teardown must precede release.
    pub retained: Vec<Arc<GuestRegionMmap>>,
}
impl<M: Mapper> Memory<M> {
    pub fn new(view: GuestMemoryMmap, mapper: M) -> Self {
        Self {
            view,
            mapper,
            blocks: BTreeMap::new(),
            retained: Vec::new(),
        }
    }
    pub fn plugged(&self, addr: u64) -> bool {
        self.blocks.contains_key(&addr)
    }
    pub fn count(&self) -> u64 {
        self.blocks.len() as u64
    }
    pub fn addresses(&self) -> Vec<u64> {
        self.blocks.keys().copied().collect()
    }
    /// Called only with every vCPU quiescent and device processing suspended.
    /// false means failure with successful rollback; Err is fatal rollback failure.
    pub fn change(&mut self, addresses: &[u64], plug: bool) -> Result<bool> {
        let mut regions = Vec::new();
        let mut next = self.view.clone();
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
                self.mapper.map(region)
            } else {
                self.mapper.unmap(region)
            };
            if result.is_err() {
                let mut failed = false;
                for r in regions[..done].iter().rev() {
                    failed |= if plug {
                        self.mapper.unmap(r)
                    } else {
                        self.mapper.map(r)
                    }
                    .is_err();
                }
                if failed {
                    self.retained.extend(regions);
                    anyhow::bail!("memory mapping rollback failed; stopping VM");
                }
                return Ok(false);
            }
        }
        self.view = next;
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

pub struct Mem {
    pub control: MemoryControl,
    pub start: u64,
    capacity: u64,
    requested: u64,
    plugged: u64,
    generation: u32,
}
impl Mem {
    pub fn new(base_mib: u64, control: MemoryControl) -> Self {
        let status = control.status();
        Self {
            start: (crate::boot::RAM + (base_mib << 20)).next_multiple_of(ALIGN),
            capacity: status.region_size_mib << 20,
            requested: status.requested_size_mib << 20,
            plugged: 0,
            generation: 0,
            control,
        }
    }
    pub fn sync_target(&mut self, ready: bool) -> bool {
        let mut status = self.control.lock();
        status.driver_ready = ready;
        let requested = status.requested_size_mib << 20;
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
        mem: &mut Memory<M>,
        pinned: &[(u64, u64)],
    ) -> Result<(u16, u16)> {
        let kind = u16::from_le_bytes(req[..2].try_into().unwrap());
        let addr = u64::from_le_bytes(req[8..16].try_into().unwrap());
        let count = u16::from_le_bytes(req[16..18].try_into().unwrap()) as u64;
        if kind > 3 {
            return Ok((3, 0));
        }
        let addresses = if kind == 2 {
            mem.addresses()
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
        let plugged = addresses.iter().filter(|&&a| mem.plugged(a)).count();
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
        let mut status = self.control.lock();
        if kind == 0 && (mem.count() + count) * 2 > status.requested_size_mib {
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
        if !mem.change(&addresses, kind == 0)? {
            return Ok((2, 0));
        }
        self.plugged = mem.count() * BLOCK;
        self.generation = self.generation.wrapping_add(1);
        status.plugged_size_mib = self.plugged >> 20;
        Ok((0, 0))
    }
    pub fn process<M: Mapper>(
        &mut self,
        queues: &mut Queues,
        mem: &mut Memory<M>,
        pinned: &[(u64, u64)],
    ) -> Result<()> {
        // Bound work so host stop/input requests remain responsive.
        for _ in 0..128 {
            let Some(chain) = queues.pop(0, &mem.view)? else {
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
                    mem.view
                        .read_slice(&mut req[offset..offset + d.len() as usize], d.addr())?;
                    offset += d.len() as usize;
                }
            }
            let (code, state) = self.request(&req, mem, &pins)?;
            let mut response = [0; 10];
            response[..2].copy_from_slice(&code.to_le_bytes());
            response[8..].copy_from_slice(&state.to_le_bytes());
            let mut offset = 0;
            for d in chain.descriptors.iter().filter(|d| d.is_write_only()) {
                let len = (d.len() as usize).min(10 - offset);
                mem.view
                    .write_slice(&response[offset..offset + len], d.addr())?;
                offset += len;
                if offset == 10 {
                    break;
                }
            }
            queues.complete(0, &mem.view, chain.head, 10)?;
        }
        Ok(())
    }
}
impl VirtioDevice for Mem {
    fn device_id(&self) -> u32 {
        24
    }
    fn features(&self) -> u64 {
        2
    }
    fn required_features(&self) -> u64 {
        2
    }
    fn generation(&self) -> u32 {
        self.generation
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        VirtioMemConfig,
        devices::mmio::{Mmio, tests::initialize},
    };
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
    fn setup() -> (Mem, Memory<Fake>) {
        (
            Mem::new(
                512,
                MemoryControl::new(&VirtioMemConfig {
                    region_size_mib: 128,
                    requested_size_mib: 128,
                }),
            ),
            Memory::new(
                GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap(),
                Fake::default(),
            ),
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
        let (mut d, mut m) = setup();
        let a = d.start;
        assert_eq!(d.request(&req(3, a, 2), &mut m, &[]).unwrap(), (0, 1));
        assert_eq!(d.request(&req(0, a, 1), &mut m, &[]).unwrap().0, 0);
        assert_eq!(d.request(&req(0, a, 1), &mut m, &[]).unwrap().0, 3);
        assert_eq!(d.request(&req(3, a, 2), &mut m, &[]).unwrap(), (0, 2));
        assert!(!m.view.check_range(GuestAddress(a + BLOCK - 8), 16));
        assert_eq!(d.request(&req(0, a + BLOCK, 1), &mut m, &[]).unwrap().0, 0);
        assert!(m.view.check_range(GuestAddress(a + BLOCK - 8), 16));
        m.view
            .write_slice(&[42; 16], GuestAddress(a + BLOCK - 8))
            .unwrap();
        d.reset().unwrap();
        let mut data = [0; 16];
        m.view
            .read_slice(&mut data, GuestAddress(a + BLOCK - 8))
            .unwrap();
        assert_eq!(data, [42; 16]);
        assert_eq!(d.request(&req(3, a, 2), &mut m, &[]).unwrap(), (0, 0));
        assert_eq!(
            d.request(&req(1, a, 1), &mut m, &[(a + 16, 4)]).unwrap().0,
            2
        );
        assert_eq!(d.request(&req(1, a, 1), &mut m, &[]).unwrap().0, 0);
        assert_eq!(d.request(&req(1, a, 2), &mut m, &[]).unwrap().0, 3);
        d.control.set_requested_mib(2).unwrap();
        assert_eq!(d.request(&req(0, a, 1), &mut m, &[]).unwrap().0, 1);
        assert_eq!(d.request(&req(2, u64::MAX, 0), &mut m, &[]).unwrap().0, 0);
        assert!(m.mapper.mapped.is_empty());
        assert_eq!(d.control.status().plugged_size_mib, 0);
    }
    #[test]
    fn invalid_requests() {
        let (mut d, mut m) = setup();
        let a = d.start;
        for (kind, addr, n) in [
            (0, a, 0),
            (0, a + 1, 1),
            (0, a - BLOCK, 1),
            (0, a + 128 * 1024 * 1024, 1),
            (0, u64::MAX, 2),
            (7, a, 1),
        ] {
            assert_eq!(d.request(&req(kind, addr, n), &mut m, &[]).unwrap().0, 3);
        }
    }
    #[test]
    fn transactional_map_and_unmap_failure() {
        let (mut d, mut m) = setup();
        let a = d.start;
        m.mapper.fail = vec![2];
        assert_eq!(d.request(&req(0, a, 2), &mut m, &[]).unwrap().0, 2);
        assert_eq!(m.count(), 0);
        assert!(m.mapper.mapped.is_empty());
        m.mapper.fail.clear();
        d.request(&req(0, a, 2), &mut m, &[]).unwrap();
        m.view.write_slice(&[91], GuestAddress(a)).unwrap();
        m.mapper.fail = vec![m.mapper.calls + 2];
        assert_eq!(d.request(&req(1, a, 2), &mut m, &[]).unwrap().0, 2);
        assert_eq!(m.count(), 2);
        assert_eq!(m.mapper.mapped.len(), 2);
        assert_eq!(m.view.read_obj::<u8>(GuestAddress(a)).unwrap(), 91);
        m.mapper.fail = vec![m.mapper.calls + 2, m.mapper.calls + 3];
        assert!(d.request(&req(1, a, 2), &mut m, &[]).is_err());
        assert_eq!(m.retained.len(), 2);
    }
    #[test]
    fn negotiation_generation_and_queue_response() {
        let (d, mut m) = setup();
        let a = d.start;
        let mut mmio = Mmio::new(d);
        mmio.write(0x24, 1, &m.view).unwrap();
        mmio.write(0x20, 1, &m.view).unwrap();
        mmio.write(0x70, 11, &m.view).unwrap();
        assert_eq!(mmio.read(0x70, 4) & 8, 0);
        mmio.write(0x70, 0, &m.view).unwrap();
        initialize(&mut mmio, &m.view);
        mmio.device.control.set_requested_mib(64).unwrap();
        assert!(mmio.device.sync_target(true));
        mmio.config_changed();
        assert_eq!(mmio.read(0xfc, 4), 1);
        assert_eq!(mmio.read(0x60, 4), 2);
        assert!(mmio.device.control.status().driver_ready);
        use virtio_queue::desc::split::Descriptor;
        m.view
            .write_obj(Descriptor::new(0x8000, 24, 1, 1), GuestAddress(0x1000))
            .unwrap();
        m.view
            .write_obj(Descriptor::new(0x9000, 10, 2, 0), GuestAddress(0x1010))
            .unwrap();
        m.view
            .write_slice(&req(0, a, 2), GuestAddress(0x8000))
            .unwrap();
        m.view.write_obj(1u16, GuestAddress(0x2002)).unwrap();
        let pins = mmio.queues.pinned(&m.view).unwrap();
        mmio.device
            .process(&mut mmio.queues, &mut m, &pins)
            .unwrap();
        assert_eq!(m.view.read_obj::<u16>(GuestAddress(0x9000)).unwrap(), 0);
        assert_eq!(m.count(), 2);
        assert_eq!(mmio.read(0x128, 8), 2 * BLOCK);
        mmio.write(0x70, 0, &m.view).unwrap();
        assert_eq!(m.count(), 2);
        assert!(!mmio.device.control.status().driver_ready);
    }
}
