pub mod block;
pub mod memory;
pub mod mmio;
pub mod net;

use crate::{net::NetDevice, storage::BlockStorage};
use anyhow::Result;
use mmio::{Queues, VirtioDevice};
use vm_memory::GuestMemoryMmap;

/// Different device types share a transport without erasing backend types.
pub enum Device<BS: BlockStorage, ND: NetDevice> {
    Block(block::Block<BS>),
    Net(net::Net<ND>),
    Mem(memory::VirtioMem),
}

impl<BS: BlockStorage, ND: NetDevice> VirtioDevice for Device<BS, ND> {
    fn required_features(&self) -> u64 {
        match self {
            Self::Mem(d) => d.required_features(),
            _ => 0,
        }
    }
    fn generation(&self) -> u32 {
        match self {
            Self::Mem(d) => d.generation(),
            _ => 0,
        }
    }
    fn device_id(&self) -> u32 {
        match self {
            Self::Block(d) => d.device_id(),
            Self::Net(d) => d.device_id(),
            Self::Mem(d) => d.device_id(),
        }
    }
    fn features(&self) -> u64 {
        match self {
            Self::Block(d) => d.features(),
            Self::Net(d) => d.features(),
            Self::Mem(d) => d.features(),
        }
    }
    fn queue_count(&self) -> usize {
        match self {
            Self::Block(d) => d.queue_count(),
            Self::Net(d) => d.queue_count(),
            Self::Mem(d) => d.queue_count(),
        }
    }
    fn read_config(&self, offset: usize, data: &mut [u8]) {
        match self {
            Self::Block(d) => d.read_config(offset, data),
            Self::Net(d) => d.read_config(offset, data),
            Self::Mem(d) => d.read_config(offset, data),
        }
    }
    fn notify(&mut self, queue: usize, queues: &mut Queues, mem: &GuestMemoryMmap) -> Result<()> {
        match self {
            Self::Block(d) => d.notify(queue, queues, mem),
            Self::Net(d) => d.notify(queue, queues, mem),
            Self::Mem(d) => d.notify(queue, queues, mem),
        }
    }
    fn poll(&mut self, queues: &mut Queues, mem: &GuestMemoryMmap) -> Result<()> {
        match self {
            Self::Block(d) => d.poll(queues, mem),
            Self::Net(d) => d.poll(queues, mem),
            Self::Mem(d) => d.poll(queues, mem),
        }
    }
    fn reset(&mut self) -> Result<()> {
        match self {
            Self::Block(d) => d.reset(),
            Self::Net(d) => d.reset(),
            Self::Mem(d) => d.reset(),
        }
    }
    fn flush(&self) -> Result<()> {
        match self {
            Self::Block(d) => d.flush(),
            Self::Net(d) => d.flush(),
            Self::Mem(d) => d.flush(),
        }
    }
}
