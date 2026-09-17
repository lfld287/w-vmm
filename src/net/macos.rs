//! Minimal vmnet.framework bindings authored against the Apple SDK.
use super::NetDevice;
use anyhow::{Context, Result, bail, ensure};
use block2::{Block, RcBlock};
use dispatch2::{DispatchQueue, DispatchRetained};
use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
    sync::mpsc,
    time::Duration,
};

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
        1005 | 1010 => bail!(
            "{operation}: vmnet permission denied ({status}); run with root privileges or an Apple-approved com.apple.vm.networking entitlement"
        ),
        _ => bail!("{operation}: vmnet error {status}"),
    }
}

fn parse_mac(value: &str) -> Result<[u8; 6]> {
    let parts: Vec<_> = value.split(':').collect();
    ensure!(parts.len() == 6, "invalid vmnet MAC address");
    let mut mac = [0; 6];
    for (byte, part) in mac.iter_mut().zip(parts) {
        ensure!(part.len() == 2, "invalid vmnet MAC address");
        *byte = u8::from_str_radix(part, 16).context("invalid vmnet MAC address")?;
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
    mtu: u16,
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
    ensure!(!dict.is_null(), "missing vmnet interface parameters");
    // SAFETY: Apple's successful completion passes a live XPC dictionary. Copy every
    // value here; no borrowed XPC object or C string escapes the callback.
    unsafe {
        let mac = parse_mac(&string(dict, vmnet_mac_address_key)?.context("missing vmnet MAC")?)?;
        let mtu = u16::try_from(xpc_dictionary_get_uint64(dict, vmnet_mtu_key))?;
        let max_frame =
            usize::try_from(xpc_dictionary_get_uint64(dict, vmnet_max_packet_size_key))?;
        ensure!(
            mtu >= 68 && max_frame >= mtu as usize + 14 && max_frame <= u16::MAX as usize + 18,
            "invalid vmnet frame capacity"
        );
        Ok(Parameters {
            mac,
            mtu,
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
    mtu: u16,
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
            ensure!(!desc.is_null(), "create vmnet description");
            xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, 1001);
            xpc_dictionary_set_uint64(desc, vmnet_mtu_key, 1500);
            let interface = vmnet_start_interface(desc, &queue, &callback);
            xpc_release(desc);
            NonNull::new(interface)
                .context("start vmnet returned no interface (check root/networking entitlement)")?
        };
        // Establish ownership before waiting/parsing, so every later failure stops it.
        let mut backend = Self {
            interface: Some(interface),
            queue,
            mac: [0; 6],
            mtu: 0,
            max_frame: 0,
            ipv4: SharedIpv4::default(),
        };
        let params = receiver
            .recv_timeout(TIMEOUT)
            .context("waiting for vmnet start")?
            .context("create vmnet shared interface (requires root or an Apple-approved networking entitlement)")?;
        backend.mac = params.mac;
        backend.mtu = params.mtu;
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
                .context("waiting for vmnet stop")?,
            "stop vmnet completion",
        )
    }

    fn interface(&self) -> Result<Interface> {
        Ok(self
            .interface
            .context("vmnet interface is closed")?
            .as_ptr())
    }
}

impl NetDevice for Vmnet {
    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
    fn mtu(&self) -> u16 {
        self.mtu
    }
    fn max_frame_len(&self) -> usize {
        self.max_frame
    }
    fn send(&mut self, frame: &[u8]) -> Result<bool> {
        ensure!(
            (14..=self.max_frame).contains(&frame.len()),
            "invalid vmnet TX frame size"
        );
        // vmnet_write only reads the iovec despite its C API taking mutable pointers.
        let mut iov = libc::iovec {
            iov_base: frame.as_ptr().cast_mut().cast(),
            iov_len: frame.len(),
        };
        let mut packet = Packet {
            size: frame.len(),
            iov: &mut iov,
            iov_count: 1,
            flags: 0,
        };
        let mut count = 1;
        let status = unsafe { vmnet_write(self.interface()?, &mut packet, &mut count) };
        if status == BUFFER_EXHAUSTED {
            return Ok(false);
        }
        check(status, "vmnet write")?;
        ensure!((0..=1).contains(&count), "invalid vmnet TX count");
        Ok(count == 1)
    }
    fn recv(&mut self, buffer: &mut [u8]) -> Result<Option<usize>> {
        ensure!(buffer.len() >= self.max_frame, "vmnet RX buffer too small");
        let mut iov = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        let mut packet = Packet {
            size: buffer.len(),
            iov: &mut iov,
            iov_count: 1,
            flags: 0,
        };
        let mut count = 1;
        check(
            unsafe { vmnet_read(self.interface()?, &mut packet, &mut count) },
            "vmnet read",
        )?;
        ensure!((0..=1).contains(&count), "invalid vmnet RX count");
        if count == 0 {
            return Ok(None);
        }
        ensure!(
            (14..=self.max_frame).contains(&packet.size),
            "invalid vmnet RX frame size"
        );
        Ok(Some(packet.size))
    }
}

impl Drop for Vmnet {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("final vmnet stop: {error:#}");
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
        assert_eq!(net.mtu(), 1500);
        assert_ne!(net.mac_address(), [0; 6]);
        net.close().unwrap();
        net.close().unwrap();
        assert!(net.recv(&mut vec![0; net.max_frame_len()]).is_err());
    }
}
