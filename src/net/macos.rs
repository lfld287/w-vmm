//! Minimal vmnet.framework bindings authored against the Apple SDK.
use super::NetDevice;
use crate::error::NetError;
use block2::{Block, RcBlock};
use dispatch2::{DispatchQueue, DispatchRetained};
use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
    sync::mpsc,
    time::Duration,
};
use vm_memory::VolatileSlice;

type Result<T> = std::result::Result<T, NetError>;

const SUCCESS: u32 = 1000;
const BUFFER_EXHAUSTED: u32 = 1007;
const TIMEOUT: Duration = Duration::from_secs(10);

type Xpc = *mut c_void;

type Interface = *mut c_void;

#[repr(C)]
struct Packet {
    size: usize,
    iov: *mut libc::iovec,
    iov_count: u32,
    flags: u32,
}

#[link(name = "vmnet", kind = "framework")]
unsafe extern "C" {
    static vmnet_operation_mode_key: *const c_char;
    static vmnet_mac_address_key: *const c_char;
    static vmnet_mtu_key: *const c_char;
    static vmnet_max_packet_size_key: *const c_char;
    static vmnet_start_address_key: *const c_char;
    static vmnet_end_address_key: *const c_char;
    static vmnet_subnet_mask_key: *const c_char;

    fn vmnet_start_interface(
        desc: Xpc,
        queue: &DispatchQueue,
        handler: &Block<dyn Fn(u32, Xpc)>,
    ) -> Interface;

    fn vmnet_stop_interface(
        interface: Interface,
        queue: &DispatchQueue,
        handler: &Block<dyn Fn(u32)>,
    ) -> u32;

    fn vmnet_read(interface: Interface, packets: *mut Packet, count: *mut i32) -> u32;

    fn vmnet_write(interface: Interface, packets: *mut Packet, count: *mut i32) -> u32;
}

unsafe extern "C" {
    fn xpc_dictionary_create(keys: *const *const c_char, values: *const Xpc, count: usize) -> Xpc;

    fn xpc_dictionary_set_uint64(dict: Xpc, key: *const c_char, value: u64);

    fn xpc_dictionary_get_uint64(dict: Xpc, key: *const c_char) -> u64;

    fn xpc_dictionary_get_string(dict: Xpc, key: *const c_char) -> *const c_char;

    fn xpc_release(object: Xpc);
}

fn check(status: u32, operation: &str) -> Result<()> {
    match status {
        SUCCESS => Ok(()),
        1005 | 1010 => Err(NetError::PermissionDenied {
            operation: operation.to_owned(),
            status,
        }),
        _ => Err(NetError::Vmnet {
            operation: operation.to_owned(),
            status,
        }),
    }
}

fn parse_mac(value: &str) -> Result<[u8; 6]> {
    let parts: Vec<_> = value.split(':').collect();
    if parts.len() != 6 {
        return Err(NetError::InvalidVmnetMacAddress);
    }
    let mut mac = [0; 6];
    for (byte, part) in mac.iter_mut().zip(parts) {
        if part.len() != 2 {
            return Err(NetError::InvalidVmnetMacAddress);
        }
        *byte = u8::from_str_radix(part, 16).map_err(NetError::MacParse)?;
    }
    Ok(mac)
}

// Called only while the vmnet completion callback owns a live parameter dictionary.
unsafe fn string(dict: Xpc, key: *const c_char) -> Result<Option<String>> {
    let value = unsafe { xpc_dictionary_get_string(dict, key) };
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(unsafe { CStr::from_ptr(value) }.to_str()?.to_owned()))
}

struct Parameters {
    mac: [u8; 6],
    max_frame: usize,
    ipv4: SharedIpv4,
}

/// Network information returned by this concrete backend, useful for manual setup.
#[derive(Debug, Clone, Default)]
pub struct SharedIpv4 {
    pub gateway: Option<String>,
    pub pool_end: Option<String>,
    pub subnet_mask: Option<String>,
}

unsafe fn parameters(dict: Xpc) -> Result<Parameters> {
    if dict.is_null() {
        return Err(NetError::MissingVmnetInterfaceParameters);
    }
    // SAFETY: Apple's successful completion passes a live XPC dictionary. Copy every
    // value here; no borrowed XPC object or C string escapes the callback.
    unsafe {
        let mac = parse_mac(&string(dict, vmnet_mac_address_key)?.ok_or(NetError::MissingMac)?)?;
        let mtu = u16::try_from(xpc_dictionary_get_uint64(dict, vmnet_mtu_key))?;
        let max_frame =
            usize::try_from(xpc_dictionary_get_uint64(dict, vmnet_max_packet_size_key))?;
        if mtu != Vmnet::MTU {
            return Err(NetError::UnexpectedMtu { mtu });
        }
        if !(mtu >= 68 && max_frame >= mtu as usize + 14 && max_frame <= u16::MAX as usize + 18) {
            return Err(NetError::InvalidVmnetFrameCapacity);
        }
        Ok(Parameters {
            mac,
            max_frame,
            ipv4: SharedIpv4 {
                gateway: string(dict, vmnet_start_address_key)?,
                pool_end: string(dict, vmnet_end_address_key)?,
                subnet_mask: string(dict, vmnet_subnet_mask_key)?,
            },
        })
    }
}

/// macOS shared-mode NAT interface. Requires root or an approved networking entitlement.
/// Owns its interface; dropping it stops I/O. Packet I/O stays on the calling thread.
pub struct Vmnet {
    interface: Option<NonNull<c_void>>,
    queue: DispatchRetained<DispatchQueue>,
    mac: [u8; 6],
    max_frame: usize,
    ipv4: SharedIpv4,
}

impl Vmnet {
    pub fn shared() -> Result<Self> {
        let queue = DispatchQueue::new("w-vmm.vmnet", None);
        let (sender, receiver) = mpsc::channel();
        // Captures are owned and thread-safe. Late completion after timeout only sends
        // to a disconnected channel, never to a stack pointer or guest memory.
        let callback = RcBlock::new(move |status, dict| {
            let result = check(status, "start vmnet").and_then(|()| unsafe { parameters(dict) });
            let _ = sender.send(result);
        });
        let interface = unsafe {
            let desc = xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0);
            if desc.is_null() {
                return Err(NetError::CreateVmnetDescription);
            }
            xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, 1001);
            xpc_dictionary_set_uint64(desc, vmnet_mtu_key, Self::MTU as u64);
            let interface = vmnet_start_interface(desc, &queue, &callback);
            xpc_release(desc);
            NonNull::new(interface).ok_or(NetError::MissingInterface)?
        };
        // Establish ownership before waiting/parsing, so every later failure stops it.
        let mut backend = Self {
            interface: Some(interface),
            queue,
            mac: [0; 6],
            max_frame: 0,
            ipv4: SharedIpv4::default(),
        };
        let params = receiver
            .recv_timeout(TIMEOUT)
            .map_err(|source| NetError::Wait {
                operation: "waiting for vmnet start",
                source,
            })?
            .map_err(|source| NetError::Start(Box::new(source)))?;
        backend.mac = params.mac;
        backend.max_frame = params.max_frame;
        backend.ipv4 = params.ipv4;
        Ok(backend)
    }

    pub fn ipv4(&self) -> &SharedIpv4 {
        &self.ipv4
    }

    /// Stop once; errors are returned to explicit callers and logged by Drop.
    pub fn close(&mut self) -> Result<()> {
        let Some(interface) = self.interface.take() else {
            return Ok(());
        };
        let (sender, receiver) = mpsc::channel();
        let callback = RcBlock::new(move |status| {
            let _ = sender.send(status);
        });
        let status = unsafe { vmnet_stop_interface(interface.as_ptr(), &self.queue, &callback) };
        check(status, "stop vmnet")?;
        check(
            receiver
                .recv_timeout(TIMEOUT)
                .map_err(|source| NetError::Wait {
                    operation: "waiting for vmnet stop",
                    source,
                })?,
            "stop vmnet completion",
        )
    }

    fn interface(&self) -> Result<Interface> {
        Ok(self.interface.ok_or(NetError::Closed)?.as_ptr())
    }
}

impl NetDevice for Vmnet {
    const MTU: u16 = 1500;

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn max_frame_len(&self) -> usize {
        self.max_frame
    }

    fn send(&mut self, frame: &[VolatileSlice<'_>]) -> Result<bool> {
        let length: usize = frame.iter().map(VolatileSlice::len).sum();
        if !(14..=self.max_frame).contains(&length) {
            return Err(NetError::InvalidVmnetTxFrameSize {
                length,
                capacity: self.max_frame,
            });
        }
        // Guards and iovecs live only through this synchronous vmnet call.
        let guards: Vec<_> = frame
            .iter()
            .filter(|s| !s.is_empty())
            .map(VolatileSlice::ptr_guard)
            .collect();
        let mut iov: Vec<_> = frame
            .iter()
            .filter(|s| !s.is_empty())
            .zip(&guards)
            .map(|(s, g)| libc::iovec {
                iov_base: g.as_ptr().cast_mut().cast(),
                iov_len: s.len(),
            })
            .collect();
        let mut packet = Packet {
            size: length,
            iov: iov.as_mut_ptr(),
            iov_count: iov.len().try_into()?,
            flags: 0,
        };
        let mut count = 1;
        let status = unsafe { vmnet_write(self.interface()?, &mut packet, &mut count) };
        if status == BUFFER_EXHAUSTED {
            return Ok(false);
        }
        check(status, "vmnet write")?;
        if !(0..=1).contains(&count) {
            return Err(NetError::InvalidVmnetTxCount { count });
        }
        Ok(count == 1)
    }

    fn recv(&mut self, buffer: &[VolatileSlice<'_>]) -> Result<Option<usize>> {
        let length: usize = buffer.iter().map(VolatileSlice::len).sum();
        if length < self.max_frame {
            return Err(NetError::VmnetRxBufferTooSmall {
                length,
                capacity: self.max_frame,
            });
        }
        let guards: Vec<_> = buffer
            .iter()
            .filter(|s| !s.is_empty())
            .map(VolatileSlice::ptr_guard_mut)
            .collect();
        let mut iov: Vec<_> = buffer
            .iter()
            .filter(|s| !s.is_empty())
            .zip(&guards)
            .map(|(s, g)| libc::iovec {
                iov_base: g.as_ptr().cast(),
                iov_len: s.len(),
            })
            .collect();
        let mut packet = Packet {
            size: length,
            iov: iov.as_mut_ptr(),
            iov_count: iov.len().try_into()?,
            flags: 0,
        };
        let mut count = 1;
        check(
            unsafe { vmnet_read(self.interface()?, &mut packet, &mut count) },
            "vmnet read",
        )?;
        if !(0..=1).contains(&count) {
            return Err(NetError::InvalidVmnetRxCount { count });
        }
        if count == 0 {
            return Ok(None);
        }
        if !(14..=self.max_frame).contains(&packet.size) {
            return Err(NetError::InvalidVmnetRxFrameSize {
                length: packet.size,
                capacity: self.max_frame,
            });
        }
        Ok(Some(packet.size))
    }
}

impl Drop for Vmnet {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("final vmnet stop: {}", crate::error::diagnostic(&error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_parsing() {
        assert_eq!(
            parse_mac("02:ab:00:19:FE:01").unwrap(),
            [2, 171, 0, 25, 254, 1]
        );
        for invalid in ["", "02:00:00", "0:00:00:00:00:00", "gg:00:00:00:00:00"] {
            assert!(parse_mac(invalid).is_err());
        }
    }

    #[test]
    #[ignore = "requires root or an approved vmnet entitlement"]
    fn shared_interface_lifecycle() {
        let mut net = Vmnet::shared().unwrap();
        assert_eq!(Vmnet::MTU, 1500);
        assert_ne!(net.mac_address(), [0; 6]);
        net.close().unwrap();
        net.close().unwrap();
        assert!(
            net.recv(&[VolatileSlice::from(
                vec![0; net.max_frame_len()].as_mut_slice()
            )])
            .is_err()
        );
    }
}
