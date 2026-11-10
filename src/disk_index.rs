//! Rebuildable open-addressing primary-key index. Slots stay on disk.
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

const SLOT: u64 = 24;

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
}

fn hash(key: i64) -> u64 {
    let mut x = u64::from_le_bytes(key.to_le_bytes());
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}
fn decode(raw: [u8; 24]) -> Option<Entry> {
    let state = u64::from_le_bytes(raw[0..8].try_into().unwrap());
    (state != 0).then(|| Entry {
        key: i64::from_le_bytes(raw[8..16].try_into().unwrap()),
        offset: u64::from_le_bytes(raw[16..24].try_into().unwrap()),
        deleted: state == 2,
    })
}
impl DiskIndex {
    pub fn create(path: PathBuf) -> Result<Self> {
        Self::sized(path, 64)
    }
    fn sized(path: PathBuf, capacity: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)?;
        file.set_len(
            capacity
                .checked_mul(SLOT)
                .ok_or_else(|| Error::new("resource_limit", "index too large"))?,
        )?;
        Ok(Self {
            path,
            file,
            capacity,
            len: 0,
        })
    }
    pub fn reader(&self) -> Result<Self> {
        Ok(Self {
            path: self.path.clone(),
            file: File::open(&self.path)?,
            capacity: self.capacity,
            len: self.len,
        })
    }
    pub fn get(&mut self, key: i64) -> Result<Option<Entry>> {
        let mut slot = hash(key) & (self.capacity - 1);
        for _ in 0..self.capacity {
            self.file.seek(SeekFrom::Start(slot * SLOT))?;
            let mut raw = [0; 24];
            self.file.read_exact(&mut raw)?;
            match decode(raw) {
                None => return Ok(None),
                Some(e) if e.key == key => return Ok(Some(e)),
                _ => {}
            }
            slot = (slot + 1) & (self.capacity - 1);
        }
        Err(Error::new("storage_error", "index has no empty slot"))
    }
    pub fn put(&mut self, entry: Entry) -> Result<()> {
        if self.len * 10 >= self.capacity * 7 {
            self.grow()?;
        }
        let mut slot = hash(entry.key) & (self.capacity - 1);
        loop {
            self.file.seek(SeekFrom::Start(slot * SLOT))?;
            let mut raw = [0; 24];
            self.file.read_exact(&mut raw)?;
            let old = decode(raw);
            if old.is_none_or(|e| e.key == entry.key) {
                if old.is_none() {
                    self.len += 1;
                }
                raw[0..8]
                    .copy_from_slice(&(if entry.deleted { 2_u64 } else { 1_u64 }).to_le_bytes());
                raw[8..16].copy_from_slice(&entry.key.to_le_bytes());
                raw[16..24].copy_from_slice(&entry.offset.to_le_bytes());
                self.file.seek(SeekFrom::Start(slot * SLOT))?;
                self.file.write_all(&raw)?;
                return Ok(());
            }
            slot = (slot + 1) & (self.capacity - 1);
        }
    }
    fn grow(&mut self) -> Result<()> {
        let capacity = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| Error::new("resource_limit", "index too large"))?;
        let path = self.path.with_extension(format!("hash{capacity}"));
        let mut next = Self::sized(path, capacity)?;
        for entry in self.entries()? {
            next.put(entry?)?;
        }
        let old = std::mem::replace(self, next);
        let path = old.path.clone();
        drop(old);
        fs::remove_file(path)?;
        Ok(())
    }
    pub fn entries(&self) -> Result<Entries> {
        Entries::open(&self.path, self.capacity)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
pub(crate) struct Entries {
    reader: BufReader<File>,
    remaining: u64,
}
impl Entries {
    fn open(path: &Path, remaining: u64) -> Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            remaining,
        })
    }
}
impl Iterator for Entries {
    type Item = Result<Entry>;
    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining > 0 {
            self.remaining -= 1;
            let mut raw = [0; 24];
            if let Err(e) = self.reader.read_exact(&mut raw) {
                self.remaining = 0;
                return Some(Err(e.into()));
            }
            if let Some(e) = decode(raw) {
                return Some(Ok(e));
            }
        }
        None
    }
}
