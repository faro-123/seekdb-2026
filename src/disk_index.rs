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
    file: Option<File>,
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
    pub fn sized(path: PathBuf, capacity: u64) -> Result<Self> {
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
            file: Some(file),
            capacity,
            len: 0,
        })
    }
    pub fn reader(&self) -> Result<Self> {
        Ok(Self {
            path: self.path.clone(),
            file: Some(File::open(&self.path)?),
            capacity: self.capacity,
            len: self.len,
        })
    }
    // Retain derived slots without retaining an idle writer. Queries open
    // their own bounded iterator; a later commit reopens the writer lazily.
    pub fn close_writer(&mut self) {
        self.file = None;
    }
    fn file(&mut self) -> Result<&mut File> {
        if self.file.is_none() {
            self.file = Some(OpenOptions::new().read(true).write(true).open(&self.path)?);
        }
        Ok(self.file.as_mut().expect("index file opened above"))
    }
    pub fn get(&mut self, key: i64) -> Result<Option<Entry>> {
        let mut slot = hash(key) & (self.capacity - 1);
        for _ in 0..self.capacity {
            self.file()?.seek(SeekFrom::Start(slot * SLOT))?;
            let mut raw = [0; 24];
            self.file()?.read_exact(&mut raw)?;
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
            self.file()?.seek(SeekFrom::Start(slot * SLOT))?;
            let mut raw = [0; 24];
            self.file()?.read_exact(&mut raw)?;
            let old = decode(raw);
            if old.is_none_or(|e| e.key == entry.key) {
                if old.is_none() {
                    self.len += 1;
                }
                return self.write_slot(slot, entry);
            }
            slot = (slot + 1) & (self.capacity - 1);
        }
    }
    fn write_slot(&mut self, slot: u64, entry: Entry) -> Result<()> {
        let mut raw = [0; SLOT as usize];
        raw[0..8].copy_from_slice(&(if entry.deleted { 2_u64 } else { 1_u64 }).to_le_bytes());
        raw[8..16].copy_from_slice(&entry.key.to_le_bytes());
        raw[16..24].copy_from_slice(&entry.offset.to_le_bytes());
        let file = self.file()?;
        file.seek(SeekFrom::Start(slot * SLOT))?;
        file.write_all(&raw)?;
        Ok(())
    }
    pub fn capacity(&self) -> u64 {
        self.capacity
    }
    fn expanded(&self) -> Result<Self> {
        let capacity = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| Error::new("resource_limit", "index too large"))?;
        let path = self.path.with_extension(format!("hash{capacity}"));
        Self::sized(path, capacity)
    }
    fn grow(&mut self) -> Result<()> {
        let mut next = self.expanded()?;
        for entry in self.entries()? {
            next.put(entry?)?;
        }
        self.replace_slots(next)
    }
    fn replace_slots(&mut self, next: Self) -> Result<()> {
        let old = std::mem::replace(self, next);
        let path = old.path.clone();
        drop(old);
        fs::remove_file(path)?;
        Ok(())
    }

    // Copies distinct keys into an empty index. Match put's growth threshold
    // and first-free-slot rule so a historical LIMIT sees the same row order.
    pub fn copy_distinct(
        &mut self,
        entries: impl IntoIterator<Item = Result<Entry>>,
    ) -> Result<()> {
        debug_assert_eq!(self.len, 0);
        let mut vacant = Vacancy::new(self.capacity);
        for entry in entries {
            let entry = entry?;
            if self.len * 10 >= self.capacity * 7 {
                let mut next = self.expanded()?;
                let mut next_vacant = Vacancy::new(next.capacity);
                for old in self.entries()? {
                    next.put_distinct(old?, &mut next_vacant)?;
                }
                self.replace_slots(next)?;
                vacant = next_vacant;
            }
            self.put_distinct(entry, &mut vacant)?;
        }
        Ok(())
    }

    fn put_distinct(&mut self, entry: Entry, vacant: &mut Option<Vacancy>) -> Result<()> {
        let Some(vacant) = vacant.as_mut() else {
            return self.put(entry);
        };
        let home = hash(entry.key) & (self.capacity - 1);
        let slot = vacant.take(home as usize);
        self.write_slot(slot as u64, entry)?;
        self.len += 1;
        Ok(())
    }
    pub fn entries(&self) -> Result<Entries> {
        Entries::open(&self.path, self.capacity)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

struct Vacancy {
    words: Vec<u64>,
}

impl Vacancy {
    fn new(capacity: u64) -> Option<Self> {
        // Only bulk copies need this scratch space. The two live generations
        // combined use at most 192 KiB while rehashing.
        const MAX_BYTES: u64 = 128 << 10;
        (capacity <= MAX_BYTES * 8).then(|| Self {
            words: vec![u64::MAX; (capacity / 64) as usize],
        })
    }

    fn take(&mut self, home: usize) -> usize {
        let word = home / 64;
        let suffix = self.words[word] & (u64::MAX << (home % 64));
        let (word, available) = if suffix != 0 {
            (word, suffix)
        } else {
            self.words
                .iter()
                .enumerate()
                .skip(word + 1)
                .chain(self.words.iter().enumerate().take(word + 1))
                .find_map(|(index, &bits)| (bits != 0).then_some((index, bits)))
                .expect("index is below its growth threshold")
        };
        let bit = available.trailing_zeros() as usize;
        self.words[word] &= !(1 << bit);
        word * 64 + bit
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_copy_preserves_slots_through_collisions_and_growth() {
        let root =
            std::env::temp_dir().join(format!("minivectordb-bulk-slots-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let mut ordinary = DiskIndex::create(root.join("ordinary.index")).unwrap();
        let mut bulk = DiskIndex::create(root.join("bulk.index")).unwrap();
        let entries: Vec<_> = (0..120_000_i64)
            .filter(|&key| hash(key) & 63 >= 60)
            .take(500)
            .enumerate()
            .map(|(ordinal, key)| Entry {
                key,
                offset: ordinal as u64,
                deleted: ordinal % 13 == 0,
            })
            .collect();
        assert_eq!(entries.len(), 500);
        for &entry in &entries {
            ordinary.put(entry).unwrap();
        }
        bulk.copy_distinct(entries.into_iter().map(Ok)).unwrap();
        assert_eq!(
            fs::read(ordinary.path()).unwrap(),
            fs::read(bulk.path()).unwrap()
        );
        drop(ordinary);
        drop(bulk);
        fs::remove_dir_all(root).unwrap();
    }
}
