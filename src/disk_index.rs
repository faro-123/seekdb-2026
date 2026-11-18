//! Rebuildable open-addressing primary-key index. Slots stay on disk.
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotFormat {
    Compact,
    FullKey,
}

impl SlotFormat {
    fn key_width(self) -> usize {
        match self {
            Self::Compact => 4,
            Self::FullKey => 8,
        }
    }

    fn width(self) -> u64 {
        (1 + self.key_width() + 8) as u64
    }

    fn fits(self, key: i64) -> bool {
        self == Self::FullKey || i32::try_from(key).is_ok()
    }

    fn read(self, reader: &mut impl Read) -> std::io::Result<Option<Entry>> {
        let key_end = 1 + self.key_width();
        let mut raw = [0; 17];
        reader.read_exact(&mut raw[..key_end + 8])?;
        if raw[0] == 0 {
            return Ok(None);
        }
        let key = match self {
            Self::Compact => i64::from(i32::from_le_bytes(raw[1..5].try_into().unwrap())),
            Self::FullKey => i64::from_le_bytes(raw[1..9].try_into().unwrap()),
        };
        Ok(Some(Entry {
            key,
            offset: u64::from_le_bytes(raw[key_end..key_end + 8].try_into().unwrap()),
            deleted: raw[0] == 2,
        }))
    }

    fn write(self, writer: &mut File, entry: Entry) -> Result<()> {
        debug_assert!(self.fits(entry.key));
        let key_end = 1 + self.key_width();
        let mut raw = [0; 17];
        raw[0] = if entry.deleted { 2 } else { 1 };
        raw[1..key_end].copy_from_slice(&entry.key.to_le_bytes()[..self.key_width()]);
        raw[key_end..key_end + 8].copy_from_slice(&entry.offset.to_le_bytes());
        writer.write_all(&raw[..key_end + 8])?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        Self::sized(path, 64, SlotFormat::Compact)
    }

    pub fn empty_like(&self, path: PathBuf) -> Result<Self> {
        Self::sized(path, self.capacity, self.format)
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
            file: Some(file),
            capacity,
            len: 0,
            format,
        })
    }
    pub fn reader(&self) -> Result<Self> {
        Ok(Self {
            path: self.path.clone(),
            file: Some(File::open(&self.path)?),
            capacity: self.capacity,
            len: self.len,
            format: self.format,
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
        let format = self.format;
        let mut slot = hash(key) & (self.capacity - 1);
        for _ in 0..self.capacity {
            let file = self.file()?;
            file.seek(SeekFrom::Start(slot * format.width()))?;
            match format.read(file)? {
                None => return Ok(None),
                Some(e) if e.key == key => return Ok(Some(e)),
                _ => {}
            }
            slot = (slot + 1) & (self.capacity - 1);
        }
        Err(Error::new("storage_error", "index has no empty slot"))
    }
    pub fn put(&mut self, entry: Entry) -> Result<()> {
        self.ensure_key(entry.key)?;
        if self.len * 10 >= self.capacity * 7 {
            self.grow()?;
        }
        let format = self.format;
        let mut slot = hash(entry.key) & (self.capacity - 1);
        loop {
            let file = self.file()?;
            file.seek(SeekFrom::Start(slot * format.width()))?;
            let old = format.read(file)?;
            if old.is_none_or(|e| e.key == entry.key) {
                self.write_slot(slot, entry)?;
                if old.is_none() {
                    self.len += 1;
                }
                return Ok(());
            }
            slot = (slot + 1) & (self.capacity - 1);
        }
    }
    fn write_slot(&mut self, slot: u64, entry: Entry) -> Result<()> {
        let format = self.format;
        let file = self.file()?;
        file.seek(SeekFrom::Start(slot * format.width()))?;
        format.write(file, entry)
    }

    fn ensure_key(&mut self, key: i64) -> Result<()> {
        if self.format.fits(key) {
            return Ok(());
        }
        let path = self
            .path
            .with_extension(format!("hash{}-full", self.capacity));
        let mut next = Self::sized(path, self.capacity, SlotFormat::FullKey)?;
        let mut reader = BufReader::new(File::open(&self.path)?);
        // Widen in the same physical slots to retain probe chains and LIMIT order.
        for slot in 0..self.capacity {
            if let Some(entry) = self.format.read(&mut reader)? {
                next.write_slot(slot, entry)?;
            }
        }
        next.len = self.len;
        drop(reader);
        self.replace_slots(next)
    }
    fn expanded(&self) -> Result<Self> {
        let capacity = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| Error::new("resource_limit", "index too large"))?;
        let path = self.path.with_extension(format!("hash{capacity}"));
        Self::sized(path, capacity, self.format)
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
        self.ensure_key(entry.key)?;
        let home = hash(entry.key) & (self.capacity - 1);
        let slot = vacant.take(home as usize);
        self.write_slot(slot as u64, entry)?;
        self.len += 1;
        Ok(())
    }
    pub fn entries(&self) -> Result<Entries> {
        Entries::open(&self.path, self.capacity, self.format)
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
    remaining_and_format: u64,
}
impl Entries {
    // File lengths are checked as capacity * width, so the count fits
    // below this bit even in the smaller format.
    const FULL_KEY: u64 = 1 << 63;

    fn open(path: &Path, remaining: u64, format: SlotFormat) -> Result<Self> {
        debug_assert!(remaining < Self::FULL_KEY);
        let format_bit = if format == SlotFormat::FullKey {
            Self::FULL_KEY
        } else {
            0
        };
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            remaining_and_format: remaining | format_bit,
        })
    }
}
impl Iterator for Entries {
    type Item = Result<Entry>;
    fn next(&mut self) -> Option<Self::Item> {
        let format = if self.remaining_and_format & Self::FULL_KEY == 0 {
            SlotFormat::Compact
        } else {
            SlotFormat::FullKey
        };
        while self.remaining_and_format & !Self::FULL_KEY != 0 {
            self.remaining_and_format -= 1;
            match format.read(&mut self.reader) {
                Ok(Some(entry)) => return Some(Ok(entry)),
                Ok(None) => {}
                Err(error) => {
                    self.remaining_and_format = 0;
                    return Some(Err(error.into()));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widening_keeps_full_offsets_tombstones_and_iteration_order() {
        let root =
            std::env::temp_dir().join(format!("minivectordb-widen-slots-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let mut index = DiskIndex::create(root.join("slots.index")).unwrap();
        for (key, offset) in [(-3, u64::MAX), (i64::from(i32::MAX), 0), (1, 1 << 63)] {
            index
                .put(Entry {
                    key,
                    offset,
                    deleted: key < 0,
                })
                .unwrap();
        }
        let original = index
            .entries()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(fs::metadata(index.path()).unwrap().len(), 64 * 13);
        let wide = Entry {
            key: i64::MIN,
            offset: 127,
            deleted: false,
        };
        index.put(wide).unwrap();
        assert_eq!(index.reader().unwrap().get(wide.key).unwrap(), Some(wide));
        let after = index
            .entries()
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            after
                .into_iter()
                .filter(|entry| entry.key != wide.key)
                .collect::<Vec<_>>(),
            original
        );
        assert_eq!(fs::metadata(index.path()).unwrap().len(), 64 * 17);
        drop(index);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bulk_copy_preserves_slots_through_collisions_and_growth() {
        let root =
            std::env::temp_dir().join(format!("minivectordb-bulk-slots-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let mut ordinary = DiskIndex::create(root.join("ordinary.index")).unwrap();
        let mut bulk = DiskIndex::create(root.join("bulk.index")).unwrap();
        let mut entries: Vec<_> = (0..120_000_i64)
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
        entries.insert(
            111,
            Entry {
                key: i64::MAX,
                offset: u64::MAX,
                deleted: false,
            },
        );
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
