//! Shared modern virtio-mmio transport and checked split-queue access.
use crate::error::DeviceError;
use std::sync::atomic::Ordering;
use virtio_bindings::bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_queue::{Queue, QueueOwnedT, QueueT, desc::split::Descriptor};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

type Result<T> = std::result::Result<T, DeviceError>;

pub(crate) const MAX_QUEUE: u16 = 128;

const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
const NEEDS_RESET: u32 = 0x40;

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
            if !q.is_valid(mem) {
                return Err(DeviceError::InvalidLiveQueue);
            }
            pins.extend([
                (q.desc_table(), u64::from(q.size()) * 16),
                (q.avail_ring(), 6 + u64::from(q.size()) * 2),
                (q.used_ring(), 6 + u64::from(q.size()) * 8),
            ]);
            let available = q
                .avail_idx(mem, Ordering::Acquire)?
                .0
                .wrapping_sub(q.next_avail());
            if available > q.size() {
                return Err(DeviceError::AvailableRingOverrun {
                    count: available,
                    size: q.size(),
                });
            }
            for i in 0..available {
                let slot = q.next_avail().wrapping_add(i) % q.size();
                let mut index: u16 =
                    mem.read_obj(GuestAddress(q.avail_ring() + 4 + u64::from(slot) * 2))?;
                let mut ended = false;
                for _ in 0..q.size() {
                    if index >= q.size() {
                        return Err(DeviceError::DescriptorIndexOutOfRange {
                            index,
                            size: q.size(),
                        });
                    }
                    let d: Descriptor =
                        mem.read_obj(GuestAddress(q.desc_table() + u64::from(index) * 16))?;
                    if !(d.flags() & !3 == 0 && mem.check_range(d.addr(), d.len() as usize)) {
                        return Err(DeviceError::InvalidPendingDescriptor {
                            address: d.addr().0,
                            length: d.len(),
                            flags: d.flags(),
                        });
                    }
                    if d.len() != 0 {
                        pins.push((d.addr().0, u64::from(d.len())));
                    }
                    if !d.has_next() {
                        ended = true;
                        break;
                    }
                    index = d.next();
                }
                if !ended {
                    return Err(DeviceError::CyclicPendingDescriptorChain);
                }
            }
        }
        Ok(pins)
    }

    /// A disabled queue has nothing available; it cannot restart before reset.
    pub(crate) fn available(&self, index: usize, mem: &GuestMemoryMmap) -> Result<u16> {
        let q = &self.rings[index];
        if !q.ready() {
            return Ok(0);
        }
        if !q.is_valid(mem) {
            return Err(DeviceError::InvalidVirtqueue);
        }
        let count = q
            .avail_idx(mem, Ordering::Acquire)?
            .0
            .wrapping_sub(q.next_avail());
        if count > q.size() {
            return Err(DeviceError::AvailableRingOverrun {
                count,
                size: q.size(),
            });
        }
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
            .ok_or_else(|| DeviceError::InvalidAvailableDescriptor)?
            .head_index();
        let mut index = head;
        let mut descriptors = Vec::new();
        // Walk raw links before virtio-queue can expand an unnegotiated indirect table.
        // Bound traversal even for guest-created cycles, and validate before any I/O.
        for _ in 0..q.size() {
            if index >= q.size() {
                return Err(DeviceError::DescriptorIndexOutOfRange {
                    index,
                    size: q.size(),
                });
            }
            let d: Descriptor =
                mem.read_obj(GuestAddress(q.desc_table() + u64::from(index) * 16))?;
            if d.refers_to_indirect_table() {
                return Err(DeviceError::UnnegotiatedIndirectDescriptor);
            }
            if !(d.flags() & !3 == 0 && mem.check_range(d.addr(), d.len() as usize)) {
                return Err(DeviceError::InvalidDescriptorFlagsRange {
                    address: d.addr().0,
                    length: d.len(),
                    flags: d.flags(),
                });
            }
            descriptors.push(d);
            if !d.has_next() {
                return Ok(Some(Chain { head, descriptors }));
            }
            index = d.next();
        }
        Err(DeviceError::CyclicDescriptorChain)
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
    strict: bool,
    rejected: u64,
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
            strict: true,
            rejected: 0,
        }
    }

    /// Lenient transports contain guest errors to this device instead of
    /// returning them; see [`Self::contain`].
    pub(crate) fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    pub(crate) fn needs_reset(&self) -> bool {
        self.status & NEEDS_RESET != 0
    }

    /// Pass through results and fatal errors. In lenient mode a guest error
    /// instead sets DEVICE_NEEDS_RESET, which stops this device's dataplane
    /// until the driver resets it; `None` reports that isolation.
    pub(crate) fn contain<T>(&mut self, result: Result<T>) -> Result<Option<T>> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(e) if !self.strict && e.is_guest() => {
                self.status |= NEEDS_RESET;
                // virtio 1.2 section 2.1.2: notify only once DRIVER_OK is set.
                if self.status & DRIVER_OK != 0 {
                    self.config_changed();
                }
                eprintln!(
                    "virtio device {}: guest error, device needs reset: {}",
                    self.device.device_id(),
                    crate::error::diagnostic(&e)
                );
                Ok(None)
            }
            Err(e) => Err(e),
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
            let result = self.device.poll(&mut self.queues, mem);
            self.contain(result)?;
        }
        Ok(())
    }

    /// `None` means the access decodes to no register or configuration field:
    /// registers are 32 bit, configuration fields 8 to 64 bit.
    pub(crate) fn read(&self, offset: u64, width: usize) -> Option<u64> {
        if !matches!(width, 1 | 2 | 4 | 8) {
            return None;
        }
        if (0x100..0x1000).contains(&offset) {
            if offset + width as u64 > 0x1000 {
                return None;
            }
            let mut data = [0; 8];
            self.device
                .read_config((offset - 0x100) as usize, &mut data[..width]);
            return Some(u64::from_le_bytes(data));
        }
        if width != 4 {
            return None;
        }
        Some(match offset {
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
        })
    }

    /// A 32-bit register write. A rejected write leaves every register
    /// unchanged; lenient mode ignores it and counts it instead of returning it.
    pub(crate) fn write(&mut self, offset: u64, value: u32, mem: &GuestMemoryMmap) -> Result<()> {
        if offset == 0x50 {
            let queue = value as usize;
            // Notifications for absent or disabled queues are ignored.
            if self.active() && self.queues.rings.get(queue).is_some_and(QueueT::ready) {
                let result = self.device.notify(queue, &mut self.queues, mem);
                self.contain(result)?;
            }
            return Ok(());
        }
        match self.configure(offset, value, mem) {
            Err(e) if !self.strict && e.is_guest() => {
                self.rejected += 1;
                if self.rejected.is_power_of_two() {
                    eprintln!(
                        "virtio device {}: ignored register write {offset:#x}={value:#x} ({} so far): {}",
                        self.device.device_id(),
                        self.rejected,
                        crate::error::diagnostic(&e)
                    );
                }
                Ok(())
            }
            result => result,
        }
    }

    /// Every error is returned before any state changes.
    fn configure(&mut self, offset: u64, value: u32, mem: &GuestMemoryMmap) -> Result<()> {
        match offset {
            0x14 => self.feature_sel = value,
            0x24 => self.driver_sel = value,
            0x30 => self.queue_sel = value,
            0x20 if self.status & FEATURES_OK == 0 && self.driver_sel < 2 => {
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
                if value & self.status != self.status {
                    return Err(DeviceError::StatusBitsCleared);
                }
                let mut status = value;
                // Leaving FEATURES_OK clear rejects the negotiated subset.
                if status & FEATURES_OK != 0
                    && (self.queues.negotiated & !self.features != 0
                        || self.queues.negotiated & (1 << VIRTIO_F_VERSION_1) == 0
                        || self.queues.negotiated & self.device.required_features()
                            != self.device.required_features())
                {
                    status &= !FEATURES_OK;
                }
                if status & DRIVER_OK != 0
                    && self.status & DRIVER_OK == 0
                    && !(status & 0xb == 0xb && self.queues.rings.iter().all(|q| q.is_valid(mem)))
                {
                    return Err(DeviceError::DriverNotReady);
                };
                self.status = status;
            }
            0x64 => self.queues.interrupt &= !value,
            0x38 | 0x44 | 0x80 | 0x84 | 0x90 | 0x94 | 0xa0 | 0xa4 => {
                if let Some(q) = self.queues.rings.get_mut(self.queue_sel as usize) {
                    // A driver may disable a queue after DRIVER_OK but not restart it.
                    let disable = offset == 0x44 && value == 0;
                    if self.status & DRIVER_OK != 0 && !disable {
                        return Err(DeviceError::QueueConfigurationAfterDriverOk);
                    }
                    if q.ready() && !disable {
                        return Err(DeviceError::QueueConfigurationWhileReady);
                    }
                    match offset {
                        0x38 => {
                            if value > MAX_QUEUE as u32 {
                                return Err(DeviceError::QueueTooLarge {
                                    size: value,
                                    max: MAX_QUEUE,
                                });
                            }
                            q.try_set_size(value as u16)?;
                        }
                        0x44 => {
                            if value > 1 {
                                return Err(DeviceError::InvalidQueueReady { value });
                            }
                            q.set_ready(value == 1);
                            if value == 1 && !q.is_valid(mem) {
                                q.set_ready(false);
                                return Err(DeviceError::InvalidQueueMemory);
                            };
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
        polls: usize,
        resets: usize,
        /// Returned by notify and poll.
        error: Option<fn() -> DeviceError>,
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
            self.error.map_or(Ok(()), |e| Err(e()))
        }

        fn poll(&mut self, _: &mut Queues, _: &GuestMemoryMmap) -> Result<()> {
            self.polls += 1;
            self.error.map_or(Ok(()), |e| Err(e()))
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
                polls: 0,
                resets: 0,
                error: None,
            }),
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap(),
        )
    }

    #[test]
    fn registers_negotiation_and_reset() {
        let (mut d, mem) = setup();
        assert_eq!(d.read(0, 4), Some(0x74726976));
        assert_eq!(d.read(8, 4), Some(42));
        assert_eq!(d.read(0x100, 4), Some(0x563412));
        assert_eq!(d.read(0x101, 2), Some(0x5634));
        assert_eq!(d.read(0x200, 8), Some(0));
        // Registers are 32 bit and configuration must not cross the window.
        assert_eq!(d.read(u64::MAX, 8), None);
        assert_eq!(d.read(0x100, 0), None);
        assert_eq!(d.read(0x100, 3), None);
        assert_eq!(d.read(0x70, 2), None);
        assert_eq!(d.read(0xffc, 8), None);
        d.write(0x50, 0, &mem).unwrap();
        assert_eq!(d.device.notifications, 0);
        initialize(&mut d, &mem);
        d.write(0x50, 1, &mem).unwrap();
        assert_eq!(d.device.notifications, 1);
        d.write(0x50, 2, &mem).unwrap();
        assert_eq!(d.device.notifications, 1);
        d.write(0x30, u32::MAX, &mem).unwrap();
        assert_eq!(d.read(0x34, 4), Some(0));
        assert_eq!(d.read(0x44, 4), Some(0));
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

    /// Snapshot of every guest-visible transport register.
    fn registers(d: &Mmio<Fake>) -> Vec<Option<u64>> {
        (0..0x100).step_by(4).map(|o| d.read(o, 4)).collect()
    }

    #[test]
    fn rejected_writes_change_nothing_and_lenient_ignores_them() {
        for strict in [true, false] {
            let (d, mem) = setup();
            let mut d = d.with_strict(strict);
            // Unsupported features with DRIVER_OK: FEATURES_OK must stay clear.
            d.write(0x70, 3, &mem).unwrap();
            d.write(0x24, 1, &mem).unwrap();
            d.write(0x20, 3, &mem).unwrap();
            // (setup writes, rejected offset, rejected value)
            type Case = (&'static [(u64, u32)], u64, u32);
            let cases: [Case; 6] = [
                (&[], 0x70, 15),
                (&[], 0x70, 1),
                // QueueReady with invalid memory restores the disabled queue.
                (&[(0x80, 0xffff_f000)], 0x44, 1),
                (&[], 0x44, 2),
                (&[], 0x38, 3),
                (&[], 0x38, 129),
            ];
            for (setup, offset, value) in cases {
                for &(o, v) in setup {
                    d.write(o, v, &mem).unwrap();
                }
                let before = registers(&d);
                let negotiated = d.queues.negotiated;
                let result = d.write(offset, value, &mem);
                assert_eq!(result.is_err(), strict, "{offset:#x}={value}");
                assert_eq!(registers(&d), before, "{offset:#x}={value}");
                assert_eq!(d.queues.negotiated, negotiated);
                assert_eq!(d.status, 3);
                assert!(!d.queues.rings[0].ready());
            }
            assert_eq!(d.rejected, if strict { 0 } else { 6 });
            // A plain FEATURES_OK write is still accepted with the bit cleared.
            d.write(0x70, 11, &mem).unwrap();
            assert_eq!(d.status, 3);
        }
    }

    #[test]
    fn queues_disable_after_driver_ok_without_restart() {
        let (mut d, mem) = setup();
        initialize(&mut d, &mem);
        d.write(0x30, 0, &mem).unwrap();
        d.write(0x44, 0, &mem).unwrap();
        assert_eq!(d.read(0x44, 4), Some(0));
        assert!(matches!(
            d.write(0x44, 1, &mem),
            Err(DeviceError::QueueConfigurationAfterDriverOk)
        ));
        assert!(d.write(0x38, 8, &mem).is_err());
        // A later status write does not revalidate the disabled queue.
        d.write(0x70, 15 | 0x80, &mem).unwrap();
        d.status = 15;
        d.write(0x50, 0, &mem).unwrap();
        assert_eq!(d.device.notifications, 0);
        d.write(0x50, 1, &mem).unwrap();
        assert_eq!(d.device.notifications, 1);
        // Disabled queues have nothing available and nothing pinned, even if corrupt.
        mem.write_obj(9u16, GuestAddress(0x2002)).unwrap();
        assert_eq!(d.queues.available(0, &mem).unwrap(), 0);
        assert_eq!(
            d.queues.pinned(&mem).unwrap(),
            [(0x4000, 128), (0x5000, 22), (0x6000, 70)]
        );
        assert!(d.queues.pop(0, &mem).unwrap().is_none());
    }

    #[test]
    fn lenient_guest_errors_isolate_and_host_errors_stay_fatal() {
        let guest: fn() -> DeviceError = || DeviceError::CyclicDescriptorChain;
        let host: fn() -> DeviceError =
            || crate::error::StorageError::Backend(std::io::Error::other("host").into()).into();
        let (d, mem) = setup();
        let mut d = d.with_strict(false);
        initialize(&mut d, &mem);
        d.device.error = Some(guest);
        d.write(0x50, 0, &mem).unwrap();
        assert_eq!(d.read(0x70, 4), Some(0x4f));
        assert_eq!(d.read(0x60, 4), Some(2));
        // The stopped dataplane sees neither notifications nor polls.
        d.write(0x50, 0, &mem).unwrap();
        d.poll(&mem).unwrap();
        assert_eq!((d.device.notifications, d.device.polls), (1, 0));
        // Status writes must keep DEVICE_NEEDS_RESET; only reset clears it.
        d.write(0x70, 15, &mem).unwrap();
        assert_eq!(d.status, 0x4f);
        d.write(0x70, 0x4f | 0x80, &mem).unwrap();
        d.write(0x70, 0, &mem).unwrap();
        assert_eq!((d.status, d.read(0x60, 4)), (0, Some(0)));
        initialize(&mut d, &mem);
        d.poll(&mem).unwrap();
        assert!(d.needs_reset());
        // Isolation before DRIVER_OK raises no configuration change.
        d.write(0x70, 0, &mem).unwrap();
        d.write(0x70, 1, &mem).unwrap();
        assert!(d.contain::<()>(Err(guest())).unwrap().is_none());
        assert_eq!((d.status, d.read(0x60, 4)), (0x41, Some(0)));
        // Host failures are fatal in lenient mode too.
        d.write(0x70, 0, &mem).unwrap();
        initialize(&mut d, &mem);
        d.device.error = Some(host);
        assert!(matches!(
            d.write(0x50, 0, &mem),
            Err(DeviceError::Storage(_))
        ));
        assert!(d.poll(&mem).is_err());
        assert!(!d.needs_reset());
        // Strict mode returns guest errors without isolating.
        let (mut d, mem) = setup();
        initialize(&mut d, &mem);
        d.device.error = Some(guest);
        assert!(d.write(0x50, 0, &mem).is_err());
        assert_eq!(d.status, 15);
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
            let error = d.queues.pop(0, &mem).err().unwrap();
            match (flags, next, addr) {
                (1, 9, _) => assert!(matches!(
                    error,
                    DeviceError::DescriptorIndexOutOfRange { index: 9, size: 8 }
                )),
                (1, 0, _) => assert!(matches!(error, DeviceError::CyclicDescriptorChain)),
                (4, _, _) => assert!(matches!(error, DeviceError::UnnegotiatedIndirectDescriptor)),
                _ => assert!(
                    matches!(error, DeviceError::InvalidDescriptorFlagsRange { address, flags: f, .. } if address == addr && f == flags)
                ),
            }
        }
        let (mut d, mem) = setup();
        initialize(&mut d, &mem);
        mem.write_obj(9u16, GuestAddress(0x2002)).unwrap();
        assert!(d.queues.available(0, &mem).is_err());
        d.write(0x70, 0, &mem).unwrap();
        assert!(matches!(
            d.write(0x38, 129, &mem),
            Err(DeviceError::QueueTooLarge {
                size: 129,
                max: 128
            })
        ));
        assert!(d.write(0x38, 3, &mem).is_err());
        d.write(0x80, 0xffff_f000, &mem).unwrap();
        assert!(d.write(0x44, 1, &mem).is_err());
    }
}
