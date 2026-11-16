//! Rebuildable open-addressing primary-key index. Slots stay on disk.
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotFormat {
    Narrow,
    Wide,
}

impl SlotFormat {
    // Zero state denotes an empty slot. The other two states mark live rows
    // and tombstones while leaving thirty bits for the byte offset.
    const NARROW_OFFSET_MAX: u64 = u32::MAX as u64 >> 2;

    fn width(self) -> u64 {
        match self {
            Self::Narrow => 8,
            Self::Wide => 24,
        }
    }

    fn fits(self, entry: Entry) -> bool {
        self == Self::Wide
            || (i32::try_from(entry.key).is_ok() && entry.offset <= Self::NARROW_OFFSET_MAX)
    }

    fn read(self, reader: &mut impl Read) -> std::io::Result<Option<Entry>> {
        match self {
            Self::Narrow => {
                let mut bytes = [0; 8];
                reader.read_exact(&mut bytes)?;
                let packed = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
                let state = packed & 3;
                Ok((state != 0).then(|| Entry {
                    key: i64::from(i32::from_le_bytes(bytes[..4].try_into().unwrap())),
                    offset: u64::from(packed >> 2),
                    deleted: state == 2,
                }))
            }
            Self::Wide => {
                let mut bytes = [0; 24];
                reader.read_exact(&mut bytes)?;
                let state = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                Ok((state != 0).then(|| Entry {
                    key: i64::from_le_bytes(bytes[8..16].try_into().unwrap()),
                    offset: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
                    deleted: state == 2,
                }))
            }
        }
    }

    fn write(self, writer: &mut File, entry: Entry) -> Result<()> {
        let state = if entry.deleted { 2_u32 } else { 1_u32 };
        match self {
            Self::Narrow => {
                let key = i32::try_from(entry.key)
                    .map_err(|_| Error::new("storage_error", "narrow index key out of range"))?;
                let offset = u32::try_from(entry.offset)
                    .ok()
                    .and_then(|n| n.checked_mul(4))
                    .ok_or_else(|| {
                        Error::new("storage_error", "narrow index offset out of range")
                    })?;
                let mut bytes = [0; 8];
                bytes[..4].copy_from_slice(&key.to_le_bytes());
                bytes[4..].copy_from_slice(&(offset | state).to_le_bytes());
                writer.write_all(&bytes)?;
            }
            Self::Wide => {
                let mut bytes = [0; 24];
                bytes[..8].copy_from_slice(&u64::from(state).to_le_bytes());
                bytes[8..16].copy_from_slice(&entry.key.to_le_bytes());
                bytes[16..24].copy_from_slice(&entry.offset.to_le_bytes());
                writer.write_all(&bytes)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub key: i64,
    pub offset: u64,
    pub deleted: bool,
}

pub(crate) struct DiskIndex {
    path: PathBuf,
    file: File,
    capacity: u64,
    len: u64,
    format: SlotFormat,
}

fn hash(key: i64) -> u64 {
    let mut x = u64::from_le_bytes(key.to_le_bytes());
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}
impl DiskIndex {
    pub fn create(path: PathBuf) -> Result<Self> {
        Self::sized(path, 64, SlotFormat::Narrow)
    }
    fn sized(path: PathBuf, capacity: u64, format: SlotFormat) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)?;
        file.set_len(
            capacity
                .checked_mul(format.width())
                .ok_or_else(|| Error::new("resource_limit", "index too large"))?,
        )?;
        Ok(Self {
            path,
            file,
            capacity,
            len: 0,
            format,
        })
    }
    pub fn reader(&self) -> Result<Self> {
        Ok(Self {
            path: self.path.clone(),
            file: File::open(&self.path)?,
            capacity: self.capacity,
            len: self.len,
            format: self.format,
        })
    }
    pub fn get(&mut self, key: i64) -> Result<Option<Entry>> {
        let mut slot = hash(key) & (self.capacity - 1);
        for _ in 0..self.capacity {
            self.file
                .seek(SeekFrom::Start(slot * self.format.width()))?;
            match self.format.read(&mut self.file)? {
                None => return Ok(None),
                Some(e) if e.key == key => return Ok(Some(e)),
                _ => {}
            }
            slot = (slot + 1) & (self.capacity - 1);
        }
        Err(Error::new("storage_error", "index has no empty slot"))
    }
    pub fn put(&mut self, entry: Entry) -> Result<()> {
        if !self.format.fits(entry) {
            self.promote()?;
        }
        if self.len * 10 >= self.capacity * 7 {
            self.grow()?;
        }
        let mut slot = hash(entry.key) & (self.capacity - 1);
        loop {
            let offset = slot * self.format.width();
            self.file.seek(SeekFrom::Start(offset))?;
            let old = self.format.read(&mut self.file)?;
            if old.is_none_or(|e| e.key == entry.key) {
                self.file.seek(SeekFrom::Start(offset))?;
                self.format.write(&mut self.file, entry)?;
                if old.is_none() {
                    self.len += 1;
                }
                return Ok(());
            }
            slot = (slot + 1) & (self.capacity - 1);
        }
    }
    fn promote(&mut self) -> Result<()> {
        let path = self
            .path
            .with_extension(format!("hash{}-wide", self.capacity));
        let mut next = Self::sized(path, self.capacity, SlotFormat::Wide)?;
        let mut reader = BufReader::new(File::open(&self.path)?);
        // Keep the probe chains and iteration order. Empty slots remain sparse.
        for slot in 0..self.capacity {
            if let Some(entry) = self.format.read(&mut reader)? {
                next.file
                    .seek(SeekFrom::Start(slot * next.format.width()))?;
                next.format.write(&mut next.file, entry)?;
            }
        }
        next.len = self.len;
        drop(reader);
        self.replace(next)
    }
    fn grow(&mut self) -> Result<()> {
        let capacity = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| Error::new("resource_limit", "index too large"))?;
        let path = self.path.with_extension(format!("hash{capacity}"));
        let mut next = Self::sized(path, capacity, self.format)?;
        for entry in self.entries()? {
            next.put(entry?)?;
        }
        self.replace(next)
    }
    fn replace(&mut self, next: Self) -> Result<()> {
        let old = std::mem::replace(self, next);
        let path = old.path.clone();
        drop(old);
        fs::remove_file(path)?;
        Ok(())
    }
    pub fn entries(&self) -> Result<Entries> {
        Entries::open(&self.path, self.capacity, self.format)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
pub(crate) struct Entries {
    reader: BufReader<File>,
    remaining: u64,
    format: SlotFormat,
}
impl Entries {
    fn open(path: &Path, remaining: u64, format: SlotFormat) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            remaining,
            format,
        })
    }
}
impl Iterator for Entries {
    type Item = Result<Entry>;
    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining > 0 {
            self.remaining -= 1;
            match self.format.read(&mut self.reader) {
                Ok(Some(entry)) => return Some(Ok(entry)),
                Ok(None) => {}
                Err(e) => {
                    self.remaining = 0;
                    return Some(Err(e.into()));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn empty() -> DiskIndex {
        static TEST_ID: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "mv-slot-{}-{}.hash",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        DiskIndex::create(path).unwrap()
    }

    fn fields(entry: Entry) -> (i64, u64, bool) {
        (entry.key, entry.offset, entry.deleted)
    }

    fn remove(index: DiskIndex) {
        let path = index.path().to_owned();
        drop(index);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn narrow_growth_preserves_entries_and_key_promotion_preserves_order() {
        let mut index = empty();
        assert_eq!(index.file.metadata().unwrap().len(), 64 * 8);
        let entries: Vec<_> = (-8000..8000)
            .filter(|&key| hash(key) & 63 == 63)
            .take(80)
            .map(|key| Entry {
                key,
                offset: SlotFormat::NARROW_OFFSET_MAX,
                deleted: key % 2 == 0,
            })
            .collect();
        for &entry in &entries {
            index.put(entry).unwrap();
        }
        assert_eq!(index.file.metadata().unwrap().len(), 128 * 8);
        let mut view = index.reader().unwrap();
        for entry in &entries {
            assert_eq!(
                fields(view.get(entry.key).unwrap().unwrap()),
                fields(*entry)
            );
        }
        drop(view);
        let before: Vec<_> = index
            .entries()
            .unwrap()
            .map(|e| fields(e.unwrap()))
            .collect();
        let wide = Entry {
            key: i64::MIN,
            offset: u64::MAX,
            deleted: false,
        };
        index.put(wide).unwrap();
        assert_eq!(index.file.metadata().unwrap().len(), 128 * 24);
        assert_eq!(fields(index.get(wide.key).unwrap().unwrap()), fields(wide));
        let after: Vec<_> = index
            .entries()
            .unwrap()
            .map(|e| fields(e.unwrap()))
            .filter(|entry| entry.0 != wide.key)
            .collect();
        assert_eq!(before, after);
        remove(index);
    }

    #[test]
    fn wide_offset_promotes_even_for_an_existing_narrow_key() {
        let mut index = empty();
        let key = i64::from(i32::MIN);
        index
            .put(Entry {
                key,
                offset: 0,
                deleted: false,
            })
            .unwrap();
        let tombstone = Entry {
            key,
            offset: SlotFormat::NARROW_OFFSET_MAX + 1,
            deleted: true,
        };
        index.put(tombstone).unwrap();
        assert_eq!(index.file.metadata().unwrap().len(), 64 * 24);
        assert_eq!(index.len, 1);
        assert_eq!(fields(index.get(key).unwrap().unwrap()), fields(tombstone));
        assert!(index.get(i64::MAX).unwrap().is_none());
        remove(index);
    }
}
