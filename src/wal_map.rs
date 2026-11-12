//! Read-only view of a bounded, committed prefix of the owned WAL.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::fs::{self, File, Metadata};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::ptr::{self, NonNull};
    use std::rc::Rc;
    use std::slice;

    const PROT_READ: i32 = 1;
    const MAP_PRIVATE: i32 = 2;

    unsafe extern "C" {
        fn mmap(
            address: *mut c_void,
            length: usize,
            protection: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(address: *mut c_void, length: usize) -> i32;
        fn geteuid() -> u32;
    }

    fn owned(metadata: &Metadata, owner: u32) -> bool {
        metadata.uid() == owner && metadata.mode() & 0o022 == 0
    }

    fn same_file(path: &Path, open: &Metadata, owner: u32) -> Option<()> {
        let entry = fs::symlink_metadata(path).ok()?;
        (entry.is_file()
            && owned(&entry, owner)
            && entry.nlink() == 1
            && entry.dev() == open.dev()
            && entry.ino() == open.ino())
        .then_some(())
    }

    pub(crate) struct WalMap {
        data: NonNull<u8>,
        len: usize,
        _lock: Rc<File>,
    }

    impl WalMap {
        /// # Safety
        /// The caller owns the WAL and holds the directory lock. The committed
        /// prefix must stay immutable for the lifetime of the mapping.
        pub unsafe fn open(
            wal: &File,
            path: &Path,
            committed: u64,
            max: usize,
            lock: &Rc<File>,
        ) -> Option<Self> {
            let len = usize::try_from(committed).ok()?;
            if len == 0 || len > isize::MAX as usize || len > max {
                return None;
            }
            // SAFETY: geteuid has no arguments and just returns the process UID.
            let owner = unsafe { geteuid() };
            let parent = path.parent()?;
            let dir = fs::symlink_metadata(parent).ok()?;
            let metadata = wal.metadata().ok()?;
            if !dir.is_dir() || !owned(&dir, owner) || metadata.len() != committed {
                return None;
            }
            same_file(path, &metadata, owner)?;
            same_file(
                &parent.join("minivectordb.lock"),
                &lock.metadata().ok()?,
                owner,
            )?;
            // SAFETY: fd is the owned, recovered regular WAL, offset zero is
            // page-aligned and length is inside its verified committed prefix.
            let ptr = unsafe {
                mmap(
                    ptr::null_mut(),
                    len,
                    PROT_READ,
                    MAP_PRIVATE,
                    wal.as_raw_fd(),
                    0,
                )
            };
            if ptr.addr() == usize::MAX {
                return None;
            }
            let Some(data) = NonNull::new(ptr.cast()) else {
                // SAFETY: successful mmap at address zero still owns a mapping.
                unsafe { munmap(ptr, len) };
                return None;
            };
            Some(Self {
                data,
                len,
                _lock: Rc::clone(lock),
            })
        }

        pub fn bytes(&self) -> &[u8] {
            // SAFETY: the directory is exclusively managed by the database.
            // Recovery truncation precedes mapping. Writes release the hot
            // snapshot before appending, and the retained lock excludes a
            // second engine until Drop unmaps this prefix.
            unsafe { slice::from_raw_parts(self.data.as_ptr(), self.len) }
        }
    }

    impl Drop for WalMap {
        fn drop(&mut self) {
            // SAFETY: this object owns this successful mmap and drops once.
            unsafe { munmap(self.data.as_ptr().cast(), self.len) };
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use linux::WalMap;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) enum WalMap {}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
impl WalMap {
    pub fn bytes(&self) -> &[u8] {
        match *self {}
    }
}
