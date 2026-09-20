//! Synchronous descriptor views. Never create ordinary references to guest bytes.
use virtio_queue::desc::split::Descriptor;
use vm_memory::{GuestMemoryBackend, GuestMemoryMmap, VolatileSlice};

pub(super) fn slices<'a>(
    mem: &'a GuestMemoryMmap,
    desc: &[Descriptor],
) -> Result<Vec<VolatileSlice<'a>>, vm_memory::GuestMemoryError> {
    desc.iter()
        .flat_map(|d| mem.get_slices(d.addr(), d.len() as usize))
        .collect()
}

pub(super) fn range<'a>(
    slices: &[VolatileSlice<'a>],
    mut skip: usize,
    mut len: usize,
) -> Vec<VolatileSlice<'a>> {
    let mut result = Vec::new();
    for s in slices {
        let offset = skip.min(s.len());
        skip -= offset;
        let n = (s.len() - offset).min(len);
        if n > 0 {
            result.push(s.subslice(offset, n).unwrap());
            len -= n;
        }
        if len == 0 {
            break;
        }
    }
    assert_eq!(len, 0);
    result
}

pub(super) fn copy_to(slices: &[VolatileSlice<'_>], mut dst: &mut [u8]) {
    for s in slices {
        if dst.is_empty() {
            break;
        }
        let n = s.len().min(dst.len());
        s.copy_to(&mut dst[..n]);
        dst = &mut dst[n..];
    }
    assert!(dst.is_empty());
}

pub(super) fn copy_from(slices: &[VolatileSlice<'_>], mut src: &[u8]) {
    for s in slices {
        if src.is_empty() {
            break;
        }
        let n = s.len().min(src.len());
        s.copy_from(&src[..n]);
        src = &src[n..];
    }
    assert!(src.is_empty());
}

pub(super) fn overlaps(slices: &[VolatileSlice<'_>]) -> bool {
    let mut ranges: Vec<_> = slices
        .iter()
        .map(|s| {
            let start = s.ptr_guard().as_ptr() as usize;
            (start, start + s.len())
        })
        .collect();
    ranges.sort_unstable();
    ranges.windows(2).any(|w| w[0].1 > w[1].0)
}
