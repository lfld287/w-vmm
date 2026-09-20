//! Host network backends, independent of the virtio transport.
use crate::error::NetError;
use vm_memory::VolatileSlice;

type Result<T> = std::result::Result<T, NetError>;

#[cfg(target_os = "macos")]
pub mod macos;

/// Nonblocking, complete Ethernet frames, excluding FCS and virtio headers.
/// Access is synchronous: implementations must not retain pointers or buffers.
/// All methods are called on the VMM thread. Implementations need not be Send/Sync.
pub trait NetDevice {
    const MTU: u16;

    fn mac_address(&self) -> [u8; 6];

    /// Maximum complete frame length; receive buffers are at least this large.
    fn max_frame_len(&self) -> usize;

    /// True accepts the whole frame; false accepts nothing and requests a retry.
    fn send(&mut self, frame: &[VolatileSlice<'_>]) -> Result<bool>;

    /// None means no packet is available and must leave the buffers unchanged.
    /// Packets must never be truncated. Buffer capacity is the sum of segment lengths.
    fn recv(&mut self, buffer: &[VolatileSlice<'_>]) -> Result<Option<usize>>;
}

impl<T: NetDevice + ?Sized> NetDevice for Box<T> {
    const MTU: u16 = T::MTU;

    fn mac_address(&self) -> [u8; 6] {
        (**self).mac_address()
    }

    fn max_frame_len(&self) -> usize {
        (**self).max_frame_len()
    }

    fn send(&mut self, frame: &[VolatileSlice<'_>]) -> Result<bool> {
        (**self).send(frame)
    }

    fn recv(&mut self, buffer: &[VolatileSlice<'_>]) -> Result<Option<usize>> {
        (**self).recv(buffer)
    }
}
