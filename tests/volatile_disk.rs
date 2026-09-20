//! Real qcow2 interoperability, including compressed input and sparse allocation.
use std::{
    fs,
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use vm_memory::VolatileSlice;
use w_vmm::storage::{BlockStorage, Disk};

struct Temp(std::path::PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn qemu(args: &[&str]) {
    let out = Command::new("qemu-img").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn read(disk: &Disk, offset: u64, bytes: &mut [u8]) {
    // Deliberately split at non-sector boundaries; aggregate length is aligned.
    let (a, b) = bytes.split_at_mut(137.min(bytes.len()));
    disk.read(offset, &[VolatileSlice::from(a), VolatileSlice::from(b)])
        .unwrap();
}

#[test]
#[ignore = "requires local qemu-img"]
fn qcow2_sparse_compressed_cross_cluster_and_persistence() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("w-vmm-volatile-{}-{unique}", std::process::id());
    let tmp = Temp(std::env::temp_dir().join(name));
    fs::create_dir(&tmp.0).unwrap();
    let raw = tmp.0.join("source.raw");
    let compressed = tmp.0.join("compressed.qcow2");
    let sparse = tmp.0.join("sparse.qcow2");
    let expected: Vec<u8> = (0..262144).map(|i| ((i / 512) % 251) as u8).collect();
    fs::write(&raw, &expected).unwrap();
    qemu(&[
        "convert",
        "-f",
        "raw",
        "-O",
        "qcow2",
        "-c",
        raw.to_str().unwrap(),
        compressed.to_str().unwrap(),
    ]);
    qemu(&["create", "-f", "qcow2", sparse.to_str().unwrap(), "1M"]);
    for path in [&compressed, &sparse] {
        let disk = Disk::open(Path::new(path), false).unwrap();
        let mut actual = vec![0xff; expected.len()];
        read(&disk, 0, &mut actual);
        if path == &compressed {
            assert_eq!(actual, expected);
        } else {
            assert!(actual.iter().all(|v| *v == 0));
        }
        // First allocation (or compressed-cluster replacement), crossing three clusters.
        let mut payload = vec![0xa5; 131072];
        let (a, b) = payload.split_at_mut(333);
        disk.write(65024, &[VolatileSlice::from(a), VolatileSlice::from(b)])
            .unwrap();
        disk.write(disk.size(), &[]).unwrap();
        disk.flush().unwrap();
        drop(disk);
        let disk = Disk::open(path, true).unwrap();
        let mut actual = vec![0; payload.len()];
        read(&disk, 65024, &mut actual);
        assert_eq!(actual, payload);
        assert!(
            disk.write(0, &[VolatileSlice::from(payload.as_mut_slice())])
                .is_err()
        );
        assert!(
            disk.read(u64::MAX, &[VolatileSlice::from(actual.as_mut_slice())])
                .is_err()
        );
        drop(disk);
        qemu(&["check", path.to_str().unwrap()]);
    }
}

#[test]
#[ignore = "requires local qemu-img"]
fn qcow2_many_segments_empty_and_overlapping_destinations() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp =
        Temp(std::env::temp_dir().join(format!("w-vmm-vectors-{}-{unique}", std::process::id())));
    fs::create_dir(&tmp.0).unwrap();
    let path = tmp.0.join("vectors.qcow2");
    let limit = unsafe { libc::sysconf(libc::_SC_IOV_MAX) };
    let limit = usize::try_from(limit).ok().filter(|&n| n > 0).unwrap_or(16);
    let len = (limit + 7) * 512;
    qemu(&[
        "create",
        "-f",
        "qcow2",
        path.to_str().unwrap(),
        &(len * 2).to_string(),
    ]);
    let disk = Disk::open(&path, false).unwrap();
    let mut expected: Vec<u8> = (0..len).map(|i| ((i / 512 + i % 17) % 251) as u8).collect();
    let mut empty = [];
    let empty = VolatileSlice::from(empty.as_mut_slice());
    let mut segments: Vec<_> = expected.chunks_mut(512).map(VolatileSlice::from).collect();
    segments.insert(0, empty);
    segments.insert(limit / 2, empty);
    segments.push(empty);
    disk.write(0, &segments).unwrap();
    let mut actual = vec![0xff; len];
    let mut targets: Vec<_> = actual.chunks_mut(512).map(VolatileSlice::from).collect();
    targets.insert(0, empty);
    targets.insert(limit / 2, empty);
    targets.push(empty);
    disk.read(0, &targets).unwrap();
    assert_eq!(actual, expected);
    disk.read(disk.size(), &[empty, empty]).unwrap();
    disk.write(disk.size(), &[empty, empty]).unwrap();
    assert!(disk.read(disk.size() + 1, &[empty]).is_err());
    assert!(disk.write(disk.size() + 1, &[empty]).is_err());

    // Both partial overlap and identical destinations must follow descriptor order.
    let mut overlap = vec![0xff; 1536];
    let whole = VolatileSlice::from(overlap.as_mut_slice());
    disk.read(
        0,
        &[
            whole.subslice(0, 1024).unwrap(),
            empty,
            whole.subslice(512, 1024).unwrap(),
            whole.subslice(512, 1024).unwrap(),
        ],
    )
    .unwrap();
    assert_eq!(&overlap[..512], &expected[..512]);
    assert_eq!(&overlap[512..], &expected[2048..3072]);

    // Invalid complete ranges cannot modify an earlier valid batch or destination.
    let mut unchanged = vec![0x39; len];
    let targets: Vec<_> = unchanged.chunks_mut(512).map(VolatileSlice::from).collect();
    assert!(disk.read(disk.size() - len as u64 + 512, &targets).is_err());
    assert!(
        disk.write(disk.size() - len as u64 + 512, &targets)
            .is_err()
    );
    assert!(unchanged.iter().all(|&b| b == 0x39));
    let mut tail = vec![0xff; len];
    read(&disk, len as u64, &mut tail);
    assert!(tail.iter().all(|&b| b == 0));
    disk.flush().unwrap();
    drop(disk);
    let disk = Disk::open(&path, true).unwrap();
    read(&disk, 0, &mut actual);
    assert_eq!(actual, expected);
    assert!(disk.write(0, &[]).is_err());
    drop(disk);
    qemu(&["check", path.to_str().unwrap()]);
}
