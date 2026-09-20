use super::buffers;
use super::mmio::{Queues, VirtioDevice, read_config};
use crate::error::DeviceError;
use crate::net::NetDevice;
use virtio_bindings::bindings::virtio_net::{VIRTIO_NET_F_MAC, VIRTIO_NET_F_MTU};
use virtio_queue::QueueT;
use vm_memory::{GuestMemoryMmap, VolatileSlice};

type Result<T> = std::result::Result<T, DeviceError>;

const HEADER: usize = 12; // VERSION_1 includes num_buffers even without MRG_RXBUF.
const BUDGET: usize = 64;

pub(crate) struct Net<ND: NetDevice> {
    backend: ND,
    config: [u8; 12],
    rx: Vec<u8>,
    pending_tx: Option<u16>,
    tx: Vec<u8>,
}

impl<ND: NetDevice> Net<ND> {
    pub(crate) fn new(backend: ND) -> Result<Self> {
        let max = backend.max_frame_len();
        if !(ND::MTU >= 68 && max >= ND::MTU as usize + 14 && max <= u16::MAX as usize + 18) {
            return Err(DeviceError::InvalidNetworkMtuFrameCapacity {
                mtu: ND::MTU,
                capacity: max,
            });
        }
        let mac = backend.mac_address();
        if !(mac[0] & 1 == 0 && mac != [0; 6]) {
            return Err(DeviceError::NetworkMacMustBeNonzeroUnicast);
        }
        let mut config = [0; 12];
        config[..6].copy_from_slice(&mac);
        config[10..12].copy_from_slice(&ND::MTU.to_le_bytes());
        Ok(Self {
            backend,
            config,
            rx: vec![0; HEADER + max],
            pending_tx: None,
            tx: Vec::with_capacity(max),
        })
    }

    fn transmit(&mut self, queues: &mut Queues, mem: &GuestMemoryMmap) -> Result<()> {
        for _ in 0..BUDGET {
            if let Some(head) = self.pending_tx {
                if !self
                    .backend
                    .send(&[VolatileSlice::from(self.tx.as_mut_slice())])?
                {
                    break;
                }
                self.pending_tx = None;
                queues.complete(1, mem, head, 0)?;
                continue;
            }
            let Some(chain) = queues.pop(1, mem)? else {
                break;
            };
            if !chain.descriptors.iter().all(|d| !d.is_write_only()) {
                return Err(DeviceError::TxDescriptorMustBeReadable);
            }
            let total = chain
                .descriptors
                .iter()
                .try_fold(0usize, |n, d| n.checked_add(d.len() as usize))
                .ok_or(DeviceError::TxLengthOverflow)?;
            if !(HEADER + 14..=self.rx.len()).contains(&total) {
                return Err(DeviceError::InvalidTxPacketSize {
                    length: total,
                    capacity: self.rx.len(),
                });
            }
            let slices = buffers::slices(mem, &chain.descriptors)?;
            let mut header = [0; HEADER];
            buffers::copy_to(&slices, &mut header);
            if header[0] != 0 || header[1] != 0 {
                return Err(DeviceError::UnnegotiatedTxChecksumGsoOffload);
            }
            let payload = buffers::range(&slices, HEADER, total - HEADER);
            if !self.backend.send(&payload)? {
                self.tx.resize(total - HEADER, 0);
                buffers::copy_to(&payload, &mut self.tx);
                self.pending_tx = Some(chain.head);
                break;
            }
            queues.complete(1, mem, chain.head, 0)?;
        }
        Ok(())
    }

    fn receive(&mut self, queues: &mut Queues, mem: &GuestMemoryMmap) -> Result<()> {
        for _ in 0..BUDGET {
            let Some(chain) = queues.pop(0, mem)? else {
                break;
            };
            if !chain.descriptors.iter().all(|d| d.is_write_only()) {
                return Err(DeviceError::RxDescriptorMustBeWritable);
            }
            let capacity = chain
                .descriptors
                .iter()
                .try_fold(0usize, |n, d| n.checked_add(d.len() as usize))
                .ok_or(DeviceError::RxLengthOverflow)?;
            let slices = buffers::slices(mem, &chain.descriptors)?;
            let direct = capacity >= self.rx.len() && !buffers::overlaps(&slices);
            let result = if direct {
                let payload = buffers::range(&slices, HEADER, self.rx.len() - HEADER);
                self.backend.recv(&payload)?
            } else {
                self.backend
                    .recv(&[VolatileSlice::from(&mut self.rx[HEADER..])])?
            };
            let Some(len) = result else {
                let q = &mut queues.rings[0];
                q.set_next_avail(q.next_avail().wrapping_sub(1));
                break;
            };
            if !(14..=self.rx.len() - HEADER).contains(&len) {
                return Err(DeviceError::InvalidBackendRxPacketSize {
                    length: len,
                    capacity: self.rx.len() - HEADER,
                });
            }
            let total = HEADER + len;
            if capacity < total {
                queues.complete(0, mem, chain.head, 0)?;
                continue;
            }
            let mut header = [0; HEADER];
            header[10..12].copy_from_slice(&1u16.to_le_bytes());
            if direct {
                buffers::copy_from(&slices, &header);
            } else {
                self.rx[..HEADER].copy_from_slice(&header);
                buffers::copy_from(&slices, &self.rx[..total]);
            }
            queues.complete(0, mem, chain.head, total as u32)?;
        }
        Ok(())
    }
}

impl<ND: NetDevice> VirtioDevice for Net<ND> {
    fn device_id(&self) -> u32 {
        1
    }

    fn features(&self) -> u64 {
        (1 << VIRTIO_NET_F_MAC) | (1 << VIRTIO_NET_F_MTU)
    }

    fn queue_count(&self) -> usize {
        2
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        read_config(&self.config, offset, data);
    }

    fn notify(&mut self, queue: usize, queues: &mut Queues, mem: &GuestMemoryMmap) -> Result<()> {
        match queue {
            0 => self.receive(queues, mem),
            1 => self.transmit(queues, mem),
            _ => unreachable!(),
        }
    }

    fn poll(&mut self, queues: &mut Queues, mem: &GuestMemoryMmap) -> Result<()> {
        self.transmit(queues, mem)?;
        self.receive(queues, mem)
    }

    fn reset(&mut self) -> Result<()> {
        self.pending_tx = None;
        self.tx.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::mmio::{Mmio, tests::initialize};
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};
    use virtio_queue::{QueueT, desc::split::Descriptor};
    use vm_memory::{Bytes, GuestAddress};

    #[derive(Default)]
    struct Fake {
        received: VecDeque<Vec<u8>>,
        sent: Vec<Vec<u8>>,
        blocked: bool,
        fail: bool,
        tx_addresses: Vec<Vec<usize>>,
        rx_addresses: Vec<Vec<usize>>,
    }

    impl NetDevice for Rc<RefCell<Fake>> {
        const MTU: u16 = 1500;

        fn mac_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }

        fn max_frame_len(&self) -> usize {
            1514
        }

        fn send(
            &mut self,
            frame: &[VolatileSlice<'_>],
        ) -> std::result::Result<bool, crate::error::NetError> {
            let mut backend = self.borrow_mut();
            backend.tx_addresses.push(
                frame
                    .iter()
                    .map(|s| s.ptr_guard().as_ptr() as usize)
                    .collect(),
            );
            if backend.fail {
                return Err(crate::error::NetError::Backend(
                    std::io::Error::other("injected send failure").into(),
                ));
            }
            if backend.blocked {
                return Ok(false);
            }
            let mut bytes = vec![0; frame.iter().map(VolatileSlice::len).sum()];
            buffers::copy_to(frame, &mut bytes);
            backend.sent.push(bytes);
            Ok(true)
        }

        fn recv(
            &mut self,
            buffer: &[VolatileSlice<'_>],
        ) -> std::result::Result<Option<usize>, crate::error::NetError> {
            let mut backend = self.borrow_mut();
            backend.rx_addresses.push(
                buffer
                    .iter()
                    .map(|s| s.ptr_guard().as_ptr() as usize)
                    .collect(),
            );
            if backend.fail {
                return Err(crate::error::NetError::Backend(
                    std::io::Error::other("injected receive failure").into(),
                ));
            }
            let Some(frame) = backend.received.pop_front() else {
                return Ok(None);
            };
            buffers::copy_from(buffer, &frame);
            Ok(Some(frame.len()))
        }
    }

    #[test]
    fn associated_mtu_and_frame_capacity() {
        struct Backend<const MTU: u16>(usize);

        impl<const MTU: u16> NetDevice for Backend<MTU> {
            const MTU: u16 = MTU;

            fn mac_address(&self) -> [u8; 6] {
                [2, 0, 0, 0, 0, 1]
            }

            fn max_frame_len(&self) -> usize {
                self.0
            }

            fn send(
                &mut self,
                _: &[VolatileSlice<'_>],
            ) -> std::result::Result<bool, crate::error::NetError> {
                unreachable!()
            }

            fn recv(
                &mut self,
                _: &[VolatileSlice<'_>],
            ) -> std::result::Result<Option<usize>, crate::error::NetError> {
                unreachable!()
            }
        }
        let d = Net::new(Box::new(Backend::<9000>(9014))).unwrap();
        let mut mtu = [0; 2];
        d.read_config(10, &mut mtu);
        assert_eq!(u16::from_le_bytes(mtu), 9000);
        assert_eq!(<Box<Backend<9000>> as NetDevice>::MTU, 9000);
        assert!(Net::new(Backend::<9000>(9013)).is_err());
        assert!(Net::new(Backend::<67>(1514)).is_err());
        assert!(Net::new(Backend::<1500>(u16::MAX as usize + 19)).is_err());
    }

    type TestNet = Mmio<Net<Rc<RefCell<Fake>>>>;

    fn setup() -> (TestNet, GuestMemoryMmap, Rc<RefCell<Fake>>) {
        let backend = Rc::new(RefCell::new(Fake::default()));
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
        let mut net = Mmio::new(Net::new(backend.clone()).unwrap());
        initialize(&mut net, &mem);
        (net, mem, backend)
    }

    fn post(net: &TestNet, mem: &GuestMemoryMmap, queue: usize, desc: &[Descriptor]) {
        let q = &net.queues.rings[queue];
        for (i, d) in desc.iter().enumerate() {
            mem.write_obj(*d, GuestAddress(q.desc_table() + i as u64 * 16))
                .unwrap();
        }
        let idx = q.next_avail();
        mem.write_obj(
            0u16,
            GuestAddress(q.avail_ring() + 4 + u64::from(idx % q.size()) * 2),
        )
        .unwrap();
        mem.write_obj(
            idx.wrapping_add(1).to_le(),
            GuestAddress(q.avail_ring() + 2),
        )
        .unwrap();
    }

    fn tx(net: &TestNet, mem: &GuestMemoryMmap) -> Vec<u8> {
        let frame = vec![0x5a; 64];
        let mut packet = vec![0; HEADER];
        packet.extend_from_slice(&frame);
        mem.write_slice(&packet[..5], GuestAddress(0x8000)).unwrap();
        mem.write_slice(&packet[5..], GuestAddress(0x9000)).unwrap();
        post(
            net,
            mem,
            1,
            &[
                Descriptor::new(0x8000, 5, 1, 1),
                Descriptor::new(0x9000, 71, 0, 0),
            ],
        );
        frame
    }

    fn used(mem: &GuestMemoryMmap, queue: usize) -> (u16, u32) {
        let base = 0x3000 + queue as u64 * 0x3000;
        (
            mem.read_obj(GuestAddress(base + 2)).unwrap(),
            mem.read_obj(GuestAddress(base + 8)).unwrap(),
        )
    }

    #[test]
    fn scatter_transmit_receive_and_irq() {
        let (mut net, mem, backend) = setup();
        assert_eq!(net.read(8, 4), 1);
        assert_eq!(net.read(0x100, 4), 2);
        assert_eq!(net.read(0x104, 2), 0x100);
        assert_eq!(net.read(0x10a, 2), 1500);
        let frame = tx(&net, &mem);
        net.write(0x50, 1, &mem).unwrap();
        assert_eq!(
            backend.borrow().sent.as_slice(),
            std::slice::from_ref(&frame)
        );
        assert_eq!(used(&mem, 1), (1, 0));
        post(
            &net,
            &mem,
            0,
            &[
                Descriptor::new(0xa000, 7, 3, 1),
                Descriptor::new(0xb000, 100, 2, 0),
            ],
        );
        net.write(0x50, 0, &mem).unwrap();
        assert_eq!(used(&mem, 0), (0, 0)); // no packet: descriptor remains available
        assert_eq!(net.queues.rings[0].next_avail(), 0);
        backend.borrow_mut().received.push_back(frame.clone());
        net.poll(&mem).unwrap(); // no guest notification needed for incoming traffic
        assert_eq!(used(&mem, 0), (1, 76));
        let mut received = [0; 76];
        mem.read_slice(&mut received[..7], GuestAddress(0xa000))
            .unwrap();
        mem.read_slice(&mut received[7..], GuestAddress(0xb000))
            .unwrap();
        assert_eq!(&received[..10], &[0; 10]);
        assert_eq!(&received[10..12], &1u16.to_le_bytes());
        assert_eq!(&received[12..], &frame);
        assert!(net.interrupt_pending());
        net.write(0x64, 1, &mem).unwrap();
        assert!(!net.interrupt_pending());
    }

    #[test]
    fn backpressure_retries_owned_frame_once_and_reset_cancels_it() {
        let (mut net, mem, backend) = setup();
        let frame = tx(&net, &mem);
        backend.borrow_mut().blocked = true;
        net.write(0x50, 1, &mem).unwrap();
        assert_eq!(used(&mem, 1), (0, 0));
        assert_eq!(net.queues.rings[1].next_avail(), 1);
        mem.write_slice(&[0xff; 71], GuestAddress(0x9000)).unwrap();
        net.poll(&mem).unwrap();
        backend.borrow_mut().blocked = false;
        net.poll(&mem).unwrap();
        net.poll(&mem).unwrap();
        assert_eq!(backend.borrow().sent, [frame]);
        assert_eq!(used(&mem, 1), (1, 0));
        tx(&net, &mem);
        backend.borrow_mut().blocked = true;
        net.poll(&mem).unwrap();
        assert!(net.device.pending_tx.is_some());
        net.write(0x70, 0, &mem).unwrap();
        assert!(net.device.pending_tx.is_none());
        backend.borrow_mut().blocked = false;
        net.poll(&mem).unwrap();
        assert_eq!(backend.borrow().sent.len(), 1);
    }

    #[test]
    fn short_rx_drops_packet_without_partial_copy() {
        let (mut net, mem, backend) = setup();
        backend.borrow_mut().received.push_back(vec![0x77; 64]);
        mem.write_slice(&[0xaa; 32], GuestAddress(0x8000)).unwrap();
        post(&net, &mem, 0, &[Descriptor::new(0x8000, 32, 2, 0)]);
        net.poll(&mem).unwrap();
        assert_eq!(used(&mem, 0), (1, 0));
        assert_eq!(mem.read_obj::<u8>(GuestAddress(0x8000)).unwrap(), 0xaa);
    }

    #[test]
    fn invalid_frames_directions_and_backend_errors() {
        for (length, flags) in [(11, 0), (25, 0), (1527, 0), (76, 2)] {
            let (mut net, mem, backend) = setup();
            post(&net, &mem, 1, &[Descriptor::new(0x8000, length, flags, 0)]);
            assert!(net.poll(&mem).is_err());
            assert!(backend.borrow().sent.is_empty());
        }
        let (mut net, mem, _) = setup();
        tx(&net, &mem);
        mem.write_obj(1u8, GuestAddress(0x8000)).unwrap();
        assert!(net.poll(&mem).is_err());
        let (mut net, mem, backend) = setup();
        post(&net, &mem, 0, &[Descriptor::new(0x8000, 2048, 0, 0)]);
        backend.borrow_mut().received.push_back(vec![0; 64]);
        assert!(net.poll(&mem).is_err());
        let (mut net, mem, backend) = setup();
        tx(&net, &mem);
        backend.borrow_mut().fail = true;
        assert!(
            matches!(net.poll(&mem), Err(DeviceError::Net(crate::error::NetError::Backend(source))) if source.downcast_ref::<std::io::Error>().is_some())
        );
        assert_eq!(used(&mem, 1), (0, 0));
    }
    #[test]
    fn direct_guest_addresses_cross_regions_empty_segments_and_rollback() {
        use vm_memory::GuestMemoryBackend;
        let backend = Rc::new(RefCell::new(Fake::default()));
        let mem = GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0), 0x9000),
            (GuestAddress(0x9000), 0x17000),
        ])
        .unwrap();
        let mut net = Mmio::new(Net::new(backend.clone()).unwrap());
        initialize(&mut net, &mem);
        let mut packet = vec![0; HEADER];
        packet.extend_from_slice(&[0x66; 64]);
        mem.write_slice(&packet, GuestAddress(0x8ff0)).unwrap();
        post(
            &net,
            &mem,
            1,
            &[
                Descriptor::new(0, 0, 1, 1),
                Descriptor::new(0x8ff0, 5, 1, 2),
                Descriptor::new(0x8ff5, 71, 0, 0),
            ],
        );
        net.poll(&mem).unwrap();
        let ptr = |addr, len| {
            mem.get_slice(GuestAddress(addr), len)
                .unwrap()
                .ptr_guard()
                .as_ptr() as usize
        };
        assert_eq!(
            backend.borrow().tx_addresses[0],
            [ptr(0x8ffc, 4), ptr(0x9000, 60)]
        );
        assert!(net.device.tx.is_empty()); // successful TX did not create a payload snapshot
        post(
            &net,
            &mem,
            0,
            &[
                Descriptor::new(0x8ff8, 5, 3, 1),
                Descriptor::new(0, 0, 3, 2),
                Descriptor::new(0x8ffd, 2043, 2, 0),
            ],
        );
        net.poll(&mem).unwrap();
        net.poll(&mem).unwrap();
        assert_eq!(net.queues.rings[0].next_avail(), 0);
        assert_eq!(used(&mem, 0), (0, 0));
        backend.borrow_mut().received.push_back(vec![0x77; 64]);
        net.poll(&mem).unwrap();
        assert_eq!(
            backend.borrow().rx_addresses.last().unwrap(),
            &[ptr(0x9004, 1514)]
        );
        let mut actual = [0; 76];
        mem.read_slice(&mut actual, GuestAddress(0x8ff8)).unwrap();
        assert_eq!(&actual[HEADER..], &[0x77; 64]);
        assert_eq!(used(&mem, 0), (1, 76));
    }

    #[test]
    fn overlapping_receive_uses_snapshot_and_invalid_tail_never_reaches_backend() {
        use vm_memory::GuestMemoryBackend;
        let (mut net, mem, backend) = setup();
        backend.borrow_mut().received.push_back((0..64).collect());
        post(
            &net,
            &mem,
            0,
            &[
                Descriptor::new(0x8000, 20, 3, 1),
                Descriptor::new(0x8000, 2048, 2, 0),
            ],
        );
        net.poll(&mem).unwrap();
        let guest = mem
            .get_slice(GuestAddress(0x8000), 2048)
            .unwrap()
            .ptr_guard()
            .as_ptr() as usize;
        assert_ne!(backend.borrow().rx_addresses[0][0], guest + HEADER);
        let mut actual = [0; 56];
        mem.read_slice(&mut actual, GuestAddress(0x8000)).unwrap();
        assert_eq!(actual.as_slice(), &(8..64).collect::<Vec<u8>>());
        let (mut net, mem, backend) = setup();
        post(
            &net,
            &mem,
            1,
            &[
                Descriptor::new(0x8000, 40, 1, 1),
                Descriptor::new(0x1fff0, 40, 0, 0),
            ],
        );
        assert!(net.poll(&mem).is_err());
        assert!(backend.borrow().tx_addresses.is_empty());
    }
}
