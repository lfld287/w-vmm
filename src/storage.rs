use crate::error::StorageError;
use imago::{
    DenyImplicitOpenGate, FormatAccess, FormatDriverBuilder,
    file::File as ImageFile,
    io_buffers::{IoVector, IoVectorMut},
    qcow2::Qcow2,
};
use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::FileExt},
    path::Path,
};
use vm_memory::VolatileSlice;

type Result<T> = std::result::Result<T, StorageError>;

/// Complete synchronous requests; implementations must not retain guest pointers.
/// Backends choose their own copying strategy; guest bytes require volatile access.
/// A failed read may have modified part of the destination.
pub trait BlockStorage {
    fn size(&self) -> u64;

    fn read_only(&self) -> bool;

    fn read(&self, offset: u64, data: &[VolatileSlice<'_>]) -> Result<()>;

    fn write(&self, offset: u64, data: &[VolatileSlice<'_>]) -> Result<()>;

    fn flush(&self) -> Result<()>;
}

// Allow a named map to contain different storage implementations when needed.
impl<T: BlockStorage + ?Sized> BlockStorage for Box<T> {
    fn size(&self) -> u64 {
        (**self).size()
    }

    fn read_only(&self) -> bool {
        (**self).read_only()
    }

    fn read(&self, offset: u64, data: &[VolatileSlice<'_>]) -> Result<()> {
        (**self).read(offset, data)
    }

    fn write(&self, offset: u64, data: &[VolatileSlice<'_>]) -> Result<()> {
        (**self).write(offset, data)
    }

    fn flush(&self) -> Result<()> {
        (**self).flush()
    }
}

pub fn bounds(size: u64, offset: u64, len: usize) -> Result<()> {
    if !offset
        .checked_add(len as u64)
        .is_some_and(|end| end <= size)
    {
        return Err(StorageError::OutOfBounds { size, offset, len });
    }
    Ok(())
}

pub fn validate_header(h: &[u8]) -> Result<u64> {
    if !(h.len() >= 104 && &h[..4] == b"QFI\xfb") {
        return Err(StorageError::InvalidHeader);
    }
    let u32at = |p| u32::from_be_bytes(h[p..p + 4].try_into().unwrap());
    let u64at = |p| u64::from_be_bytes(h[p..p + 8].try_into().unwrap());
    let version = u32at(4);
    if !(version == 2 || version == 3) {
        return Err(StorageError::UnsupportedVersion { version });
    }
    if !(u64at(8) == 0 && u32at(16) == 0) {
        return Err(StorageError::BackingChainsUnsupported);
    }
    if !(9..=21).contains(&u32at(20)) {
        return Err(StorageError::InvalidQcow2ClusterSize { bits: u32at(20) });
    }
    if u32at(32) != 0 {
        return Err(StorageError::EncryptedImagesUnsupported);
    }
    if !(u32at(60) == 0 && u64at(64) == 0) {
        return Err(StorageError::InternalSnapshotsUnsupported);
    }
    if version == 3 {
        if u64at(72) != 0 {
            return Err(StorageError::IncompatibleFeatures {
                features: u64at(72),
            });
        }
        if !(u32at(100) >= 104 && u32at(100) as u64 <= 1u64 << u32at(20)) {
            return Err(StorageError::InvalidHeaderLength { length: u32at(100) });
        }
    }
    let size = u64at(24);
    if !(size > 0 && size.is_multiple_of(512)) {
        return Err(StorageError::InvalidCapacity { size });
    }
    Ok(size)
}

fn iov_max() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        let limit = unsafe { libc::sysconf(libc::_SC_IOV_MAX) };
        usize::try_from(limit).ok().filter(|&n| n > 0).unwrap_or(16)
    })
}

fn overlaps(a: &VolatileSlice<'_>, b: &VolatileSlice<'_>) -> bool {
    let a_start = a.ptr_guard().as_ptr() as usize;
    let b_start = b.ptr_guard().as_ptr() as usize;
    a_start < b_start.saturating_add(b.len()) && b_start < a_start.saturating_add(a.len())
}

pub struct Disk {
    image: FormatAccess<ImageFile>,
    lock: File,
    readonly: bool,
    size: u64,
    path: std::path::PathBuf,
}

impl Disk {
    fn transfer(&self, offset: u64, data: &[VolatileSlice<'_>], write: bool) -> Result<()> {
        let operation = if write { "qcow2 write" } else { "qcow2 read" };
        let context = |source| StorageError::Operation {
            operation,
            path: self.path.clone(),
            source,
        };
        let len = data.iter().try_fold(0usize, |len, segment| {
            len.checked_add(segment.len()).ok_or_else(|| {
                context(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "disk request length overflow",
                ))
            })
        })?;
        // Validate the complete request before any batch can change the disk or guest.
        bounds(self.size, offset, len)?;
        let limit = iov_max();
        let mut segments = data.iter().filter(|segment| !segment.is_empty()).peekable();
        let mut offset = offset;
        while segments.peek().is_some() {
            let mut batch: Vec<&VolatileSlice<'_>> = Vec::new();
            let mut bytes = 0;
            while let Some(&segment) = segments.peek() {
                if batch.len() == limit
                    || (!write && batch.iter().any(|other| overlaps(segment, other)))
                {
                    break;
                }
                bytes += segment.len();
                batch.push(segments.next().unwrap());
            }
            // Only descriptors are collected here. imago owns the conversion and buffering.
            if write {
                let (vector, guard) = IoVector::from_volatile_slice(batch.iter().copied());
                let result = self.image.writev(vector, offset);
                drop(guard);
                result.map_err(context)?;
            } else {
                let (vector, guard) = IoVectorMut::from_volatile_slice(batch.iter().copied());
                let result = self.image.readv(vector, offset);
                drop(guard);
                result.map_err(context)?;
            }
            offset += bytes as u64;
        }
        Ok(())
    }

    pub fn open(path: &Path, readonly: bool) -> Result<Self> {
        let lock = OpenOptions::new()
            .read(true)
            .write(!readonly)
            .open(path)
            .map_err(|source| StorageError::Operation {
                operation: "open",
                path: path.to_owned(),
                source,
            })?;
        if !(lock
            .metadata()
            .map_err(|source| StorageError::Operation {
                operation: "metadata",
                path: path.to_owned(),
                source,
            })?
            .is_file())
        {
            return Err(StorageError::NotRegularFile);
        }
        let mode = if readonly {
            libc::LOCK_SH
        } else {
            libc::LOCK_EX
        };
        if unsafe { libc::flock(lock.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error()).map_err(|source| {
                StorageError::Operation {
                    operation: "disk is already locked",
                    path: path.to_owned(),
                    source,
                }
            });
        }
        let mut h = [0; 104];
        lock.read_exact_at(&mut h, 0)
            .map_err(|source| StorageError::Operation {
                operation: "read qcow2 header",
                path: path.to_owned(),
                source,
            })?;
        let size = validate_header(&h)?;
        // A duplicated descriptor keeps validation, the advisory lock and I/O on the same inode.
        let file =
            ImageFile::try_from(lock.try_clone().map_err(|source| StorageError::Operation {
                operation: "duplicate disk descriptor",
                path: path.to_owned(),
                source,
            })?)
            .map_err(|source| StorageError::Operation {
                operation: "create image file",
                path: path.to_owned(),
                source,
            })?;
        let image = Qcow2::<ImageFile>::builder(file)
            .write(!readonly)
            .open(DenyImplicitOpenGate::default())
            .map_err(|source| StorageError::Operation {
                operation: "open qcow2 metadata",
                path: path.to_owned(),
                source,
            })?;
        let image = FormatAccess::new(image);
        if image.size() != size {
            return Err(StorageError::CapacityMismatch {
                expected: size,
                actual: image.size(),
            });
        }
        Ok(Self {
            image,
            lock,
            readonly,
            size,
            path: path.to_owned(),
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

    fn read(&self, offset: u64, data: &[VolatileSlice<'_>]) -> Result<()> {
        self.transfer(offset, data, false)
    }

    fn write(&self, offset: u64, data: &[VolatileSlice<'_>]) -> Result<()> {
        if self.readonly {
            return Err(StorageError::ReadOnly);
        }
        self.transfer(offset, data, true)
    }

    fn flush(&self) -> Result<()> {
        if !self.readonly {
            self.image
                .flush()
                .map_err(|source| StorageError::Operation {
                    operation: "qcow2 cache flush",
                    path: self.path.clone(),
                    source,
                })?;
            self.image
                .sync()
                .map_err(|source| StorageError::Operation {
                    operation: "qcow2 host sync",
                    path: self.path.clone(),
                    source,
                })?;
            self.lock
                .sync_all()
                .map_err(|source| StorageError::Operation {
                    operation: "disk fsync",
                    path: self.path.clone(),
                    source,
                })?;
        }
        Ok(())
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            eprintln!("final disk flush: {}", crate::error::diagnostic(&e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_bounds() {
        assert!(matches!(
            bounds(4096, 4090, 7),
            Err(StorageError::OutOfBounds {
                size: 4096,
                offset: 4090,
                len: 7
            })
        ));
        assert!(matches!(
            bounds(4096, u64::MAX, 1),
            Err(StorageError::OutOfBounds {
                offset: u64::MAX,
                ..
            })
        ));
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
        assert!(matches!(
            validate_header(&h[..72]),
            Err(StorageError::InvalidHeader)
        ));
    }
    #[test]
    fn open_context_preserves_io_source() {
        use std::error::Error;
        let path = Path::new("/dev/null/w-vmm.qcow2");
        let error = Disk::open(path, true).err().unwrap();
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap();
        assert!(source.raw_os_error().is_some());
        assert!(
            matches!(&error, StorageError::Operation { operation: "open", path: actual, .. } if actual == path)
        );
        assert!(error.to_string().contains(&source.to_string()));
    }
}
