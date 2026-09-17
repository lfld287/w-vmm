//! Host network backends, independent of the virtio transport.
use anyhow::Result;

#[cfg(target_os = "macos")]
pub mod macos;

/// Nonblocking, complete Ethernet frames, excluding FCS and virtio headers.
/// All methods are called on the VMM thread. Implementations need not be Send/Sync.
pub trait NetDevice {
    fn mac_address(&self) -> [u8; 6];
    const MTU: u16;
    /// Maximum complete frame length; receive buffers are at least this large.
    fn max_frame_len(&self) -> usize;
    /// True accepts the whole frame; false accepts nothing and requests a retry.
    fn send(&mut self, frame: &[u8]) -> Result<bool>;
    /// None means no packet is available. Packets must never be truncated.
    fn recv(&mut self, buffer: &mut [u8]) -> Result<Option<usize>>;
}

impl<T: NetDevice + ?Sized> NetDevice for Box<T> {
    fn mac_address(&self) -> [u8; 6] {
        (**self).mac_address()
    }
    const MTU: u16 = T::MTU;
    fn max_frame_len(&self) -> usize {
        (**self).max_frame_len()
    }
    fn send(&mut self, frame: &[u8]) -> Result<bool> {
        (**self).send(frame)
    }
    fn recv(&mut self, buffer: &mut [u8]) -> Result<Option<usize>> {
        (**self).recv(buffer)
    }
}
