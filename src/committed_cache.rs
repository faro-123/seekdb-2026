//! Owned, bounded bytes of the complete validated WAL, shared with read views.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::rc::Rc;

use crate::Result;

pub(crate) const MAX_BYTES: usize = 4 << 20;
const INITIAL_BYTES: usize = 64 << 10;
const OWNER_BYTES: usize = 128;

/// Leave room for the old and new buffers during growth, as well as query work.
pub(crate) fn limit_for_hot_budget(hot: usize) -> usize {
    let room = hot.saturating_sub(1 << 20).saturating_sub(2 * OWNER_BYTES);
    let maximum = room / 3 * 2;
    if maximum == 0 {
        return 0;
    }
    let power = usize::BITS - 1 - maximum.leading_zeros();
    (1_usize << power).min(MAX_BYTES)
}

pub(crate) struct CommittedCache {
    bytes: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct CommittedView(Rc<CommittedCache>);

impl CommittedView {
    pub fn bytes(&self) -> &[u8] {
        &self.0.bytes
    }
}

impl AsRef<[u8]> for CommittedView {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

impl CommittedCache {
    pub fn refresh(
        previous: Option<Rc<Self>>,
        file: &File,
        end: u64,
        limit: usize,
    ) -> Result<Option<Rc<Self>>> {
        let Ok(end) = usize::try_from(end) else {
            return Ok(None);
        };
        let payload_limit = limit.min(MAX_BYTES).saturating_sub(OWNER_BYTES);
        if end > payload_limit || payload_limit == 0 {
            return Ok(None);
        }
        let mut owner = previous.unwrap_or_else(|| Rc::new(Self { bytes: Vec::new() }));
        // A retained reader continues to own its complete generation. Do not
        // copy it to make room for a new one.
        let Some(cache) = Rc::get_mut(&mut owner) else {
            return Ok(None);
        };
        if cache.bytes.len() > end {
            return Ok(None);
        }
        if end > cache.bytes.capacity() {
            let capacity = end
                .next_power_of_two()
                .max(INITIAL_BYTES)
                .min(payload_limit);
            // At most 2 MiB old + 4 MiB new + the query workspace. The hot
            // snapshot is released before any commit can reach this point.
            let mut next = Vec::with_capacity(capacity);
            next.extend_from_slice(&cache.bytes);
            cache.bytes = next;
        }
        let start = cache.bytes.len();
        cache.bytes.resize(end, 0);
        let mut reader = file.try_clone()?;
        reader.seek(SeekFrom::Start(start as u64))?;
        reader.read_exact(&mut cache.bytes[start..])?;
        Ok(Some(owner))
    }

    pub fn view(owner: &Rc<Self>) -> CommittedView {
        CommittedView(Rc::clone(owner))
    }

    pub fn allocated(&self) -> usize {
        self.bytes.capacity() + OWNER_BYTES
    }
}

/// Release clean whole pages only after the complete prefix has another owner.
pub(crate) fn release_file_pages(file: &File, end: u64) {
    release_file_range(file, 0, end);
}

/// Advance a consumed prefix from `start` to `end`. Keep the trailing
/// partial page until a later range completes it.
pub(crate) fn release_file_range(file: &File, start: u64, end: u64) {
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    {
        use std::os::fd::AsRawFd;
        #[allow(unsafe_code)]
        unsafe extern "C" {
            fn getpagesize() -> i32;
            fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
        }
        // SAFETY: the fd is live. The advisory call changes cache residency,
        // never file contents. Keep the partial page used by the next append.
        #[allow(unsafe_code)]
        unsafe {
            let page = getpagesize();
            if page > 0 {
                let page = page as u64;
                let start = start / page * page;
                let end = end / page * page;
                if start < end
                    && let (Ok(start), Ok(end)) = (i64::try_from(start), i64::try_from(end))
                {
                    let _ = posix_fadvise(file.as_raw_fd(), start, end - start, 4);
                }
            }
        }
    }
    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    let _ = (file, start, end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

    #[test]
    fn readers_pin_old_bytes_and_oversized_wal_falls_back() {
        let path = std::env::temp_dir().join(format!(
            "mv-committed-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.write_all(b"old").unwrap();
        let owner = CommittedCache::refresh(None, &file, 3, MAX_BYTES)
            .unwrap()
            .unwrap();
        let old = CommittedCache::view(&owner);
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"-new").unwrap();
        assert!(
            CommittedCache::refresh(Some(owner), &file, 7, MAX_BYTES)
                .unwrap()
                .is_none()
        );
        assert_eq!(old.bytes(), b"old");
        let fresh = CommittedCache::refresh(None, &file, 7, MAX_BYTES)
            .unwrap()
            .unwrap();
        assert_eq!(CommittedCache::view(&fresh).bytes(), b"old-new");
        assert!(
            CommittedCache::refresh(Some(fresh), &file, MAX_BYTES as u64, MAX_BYTES)
                .unwrap()
                .is_none()
        );
        drop((old, file));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cache_growth_stays_inside_reduced_hot_budgets() {
        for hot in [0, 1 << 20, 2 << 20, 3 << 20, 6 << 20, 8 << 20] {
            let cap = limit_for_hot_budget(hot);
            if cap != 0 {
                assert!(cap + cap / 2 + 2 * OWNER_BYTES + (1 << 20) <= hot);
            }
        }
    }
}
