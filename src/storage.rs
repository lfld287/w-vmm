use anyhow::{Context, Result, ensure};
use imago::{
    DenyImplicitOpenGate, FormatAccess, FormatDriverBuilder, file::File as ImageFile, qcow2::Qcow2,
};
use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::FileExt},
    path::Path,
};

pub trait BlockStorage {
    fn size(&self) -> u64;

    fn read_only(&self) -> bool;

    fn read(&self, offset: u64, data: &mut [u8]) -> Result<()>;

    fn write(&self, offset: u64, data: &[u8]) -> Result<()>;

    fn flush(&self) -> Result<()>;
}

pub fn bounds(size: u64, offset: u64, len: usize) -> Result<()> {
    ensure!(
        offset
            .checked_add(len as u64)
            .is_some_and(|end| end <= size),
        "disk request out of bounds"
    );
    Ok(())
}

pub fn validate_header(h: &[u8]) -> Result<u64> {
    ensure!(
        h.len() >= 104 && &h[..4] == b"QFI\xfb",
        "invalid/truncated qcow2 header"
    );
    let u32at = |p| u32::from_be_bytes(h[p..p + 4].try_into().unwrap());
    let u64at = |p| u64::from_be_bytes(h[p..p + 8].try_into().unwrap());
    let version = u32at(4);
    ensure!(version == 2 || version == 3, "only qcow2 v2/v3 supported");
    ensure!(
        u64at(8) == 0 && u32at(16) == 0,
        "backing chains unsupported"
    );
    ensure!((9..=21).contains(&u32at(20)), "invalid qcow2 cluster size");
    ensure!(u32at(32) == 0, "encrypted images unsupported");
    ensure!(
        u32at(60) == 0 && u64at(64) == 0,
        "internal snapshots unsupported"
    );
    if version == 3 {
        ensure!(
            u64at(72) == 0,
            "qcow2 incompatible features (dirty/corrupt/external/compressed/extended) unsupported; run qemu-img check"
        );
        ensure!(
            u32at(100) >= 104 && u32at(100) as u64 <= 1u64 << u32at(20),
            "invalid header length"
        );
    }
    let size = u64at(24);
    ensure!(
        size > 0 && size.is_multiple_of(512),
        "disk capacity must be a positive multiple of 512"
    );
    Ok(size)
}

pub struct Disk {
    image: FormatAccess<ImageFile>,
    lock: File,
    readonly: bool,
    size: u64,
}

impl Disk {
    pub fn open(path: &Path, readonly: bool) -> Result<Self> {
        let lock = OpenOptions::new()
            .read(true)
            .write(!readonly)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        ensure!(lock.metadata()?.is_file(), "disk must be a regular file");
        let mode = if readonly {
            libc::LOCK_SH
        } else {
            libc::LOCK_EX
        };
        if unsafe { libc::flock(lock.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error()).context("disk is already locked");
        }
        let mut h = [0; 104];
        lock.read_exact_at(&mut h, 0).context("read qcow2 header")?;
        let size = validate_header(&h)?;
        // A duplicated descriptor keeps validation, the advisory lock and I/O on the same inode.
        let file = ImageFile::try_from(lock.try_clone()?)?;
        let image = Qcow2::<ImageFile>::builder(file)
            .write(!readonly)
            .open(DenyImplicitOpenGate::default())
            .context("open qcow2 metadata")?;
        let image = FormatAccess::new(image);
        ensure!(image.size() == size, "qcow2 capacity mismatch");
        Ok(Self {
            image,
            lock,
            readonly,
            size,
        })
    }
}

impl BlockStorage for Disk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_only(&self) -> bool {
        self.readonly
    }

    fn read(&self, offset: u64, data: &mut [u8]) -> Result<()> {
        bounds(self.size, offset, data.len())?;
        self.image.read(data, offset).context("qcow2 read")
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
        ensure!(!self.readonly, "disk is read-only");
        bounds(self.size, offset, data.len())?;
        self.image.write(data, offset).context("qcow2 write")
    }

    fn flush(&self) -> Result<()> {
        if !self.readonly {
            self.image.flush().context("qcow2 cache flush")?;
            self.image.sync().context("qcow2 host sync")?;
            self.lock.sync_all().context("disk fsync")?;
        }
        Ok(())
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            eprintln!("final disk flush: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_bounds() {
        assert!(bounds(4096, 4090, 7).is_err());
        assert!(bounds(4096, u64::MAX, 1).is_err());
        assert!(bounds(4096, 0, 4096).is_ok());
    }

    #[test]
    fn unsupported_headers() {
        let mut h = [0; 104];
        h[..4].copy_from_slice(b"QFI\xfb");
        h[7] = 3;
        h[23] = 16;
        h[30] = 2;
        h[103] = 104;
        assert_eq!(validate_header(&h).unwrap(), 512);
        for p in [8, 16, 32, 60, 64, 72] {
            let mut bad = h;
            bad[p] = 1;
            assert!(validate_header(&bad).is_err(), "field {p}");
        }
        assert!(validate_header(&h[..72]).is_err());
    }
}
