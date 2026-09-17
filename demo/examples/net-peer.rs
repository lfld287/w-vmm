//! Local test peer demonstrating NetDevice injection. No host network or privileges.
//! Run the signed binary as: net-peer run [disk.qcow2 ...]
//! Guest: ip link set eth0 up; ip addr add 192.0.2.2/24 dev eth0; ping 192.0.2.1
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
};
use w_vmm::{VmConfig, Vmm, net::NetDevice, storage::Disk};
use w_vmm_demo::terminal::Terminal;

const PEER_MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];
const PEER_IP: [u8; 4] = [192, 0, 2, 1];
#[derive(Default)]
struct Peer {
    replies: VecDeque<Vec<u8>>,
}

fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = data
        .chunks(2)
        .map(|c| (u16::from(c[0]) << 8 | u16::from(*c.get(1).unwrap_or(&0))) as u32)
        .sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

impl NetDevice for Peer {
    fn mac_address(&self) -> [u8; 6] {
        [2, 0, 0, 0, 0, 1]
    }
    fn mtu(&self) -> u16 {
        1500
    }
    fn max_frame_len(&self) -> usize {
        1514
    }
    fn send(&mut self, frame: &[u8]) -> Result<bool> {
        if self.replies.len() == 128 {
            return Ok(false);
        }
        if frame.len() < 42 {
            return Ok(true);
        }
        let mut reply = frame.to_vec();
        match &frame[12..14] {
            [8, 6] if frame[14..22] == [0, 1, 8, 0, 6, 4, 0, 1] && frame[38..42] == PEER_IP => {
                reply[20..22].copy_from_slice(&2u16.to_be_bytes());
                reply[32..38].copy_from_slice(&frame[22..28]);
                reply[38..42].copy_from_slice(&frame[28..32]);
                reply[22..28].copy_from_slice(&PEER_MAC);
                reply[28..32].copy_from_slice(&PEER_IP);
            }
            [8, 0]
                if frame[14] == 0x45
                    && frame[23] == 1
                    && frame[30..34] == PEER_IP
                    && frame[34..36] == [8, 0] =>
            {
                let total = u16::from_be_bytes([frame[16], frame[17]]) as usize;
                ensure!(
                    total >= 28 && total + 14 <= frame.len(),
                    "invalid test-peer IPv4 length"
                );
                ensure!(
                    u16::from_be_bytes([frame[20], frame[21]]) & 0x3fff == 0,
                    "test peer does not reassemble fragments"
                );
                ensure!(
                    checksum(&frame[14..34]) == 0 && checksum(&frame[34..14 + total]) == 0,
                    "invalid guest packet checksum"
                );
                reply.truncate(total + 14);
                reply[26..30].copy_from_slice(&PEER_IP);
                reply[30..34].copy_from_slice(&frame[26..30]);
                reply[34] = 0;
                reply[36..38].fill(0);
                let sum = checksum(&reply[34..]);
                reply[36..38].copy_from_slice(&sum.to_be_bytes());
                // Swapping the IPv4 addresses preserves the header checksum.
            }
            _ => return Ok(true),
        }
        reply[..6].copy_from_slice(&frame[6..12]);
        reply[6..12].copy_from_slice(&PEER_MAC);
        self.replies.push_back(reply);
        Ok(true)
    }
    fn recv(&mut self, buffer: &mut [u8]) -> Result<Option<usize>> {
        let Some(frame) = self.replies.pop_front() else {
            return Ok(None);
        };
        ensure!(buffer.len() >= frame.len(), "test peer RX buffer too small");
        buffer[..frame.len()].copy_from_slice(&frame);
        Ok(Some(frame.len()))
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    ensure!(
        args.next().as_deref() == Some("run"),
        "usage: net-peer run [disk.qcow2 ...]"
    );
    let blocks = args
        .enumerate()
        .map(|(i, path)| Disk::open(Path::new(&path), false).map(|d| (format!("disk{i}"), d)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let terminal = Terminal::new()?;
    eprintln!("w-vmm: 1 vCPU, 512 MiB; Ctrl-] exits");
    Vmm::new(VmConfig::default()).run(blocks, Some(Peer::default()), terminal)
}
