//! One bounded sequence of staged positions, with the general disk fallback.

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::disk_index::{DiskIndex, Entries, Entry};

// Bound the number of slots that can be materialized at one fallback point.
const MAX_RUN_ROWS: u64 = 4096;

#[derive(Clone, Copy)]
pub(crate) struct Run {
    first_key: i64,
    first_offset: u64,
    stride: u64,
    count: u64,
}

impl Run {
    fn new(entry: Entry) -> Self {
        Self {
            first_key: entry.key,
            first_offset: entry.offset,
            stride: 0,
            count: 1,
        }
    }

    fn append(&mut self, entry: Entry) -> bool {
        if entry.deleted
            || self.count == MAX_RUN_ROWS
            || self.first_key.checked_add(self.count as i64) != Some(entry.key)
        {
            return false;
        }
        let Some(delta) = entry.offset.checked_sub(self.first_offset) else {
            return false;
        };
        if delta == 0 {
            return false;
        }
        if self.count == 1 {
            self.stride = delta;
        } else if self.stride.checked_mul(self.count) != Some(delta) {
            return false;
        }
        self.count += 1;
        true
    }

    fn entry(self, ordinal: u64) -> Entry {
        debug_assert!(ordinal < self.count);
        // append checked both the key and offset of the last ordinal.
        Entry {
            key: self.first_key + ordinal as i64,
            offset: self.first_offset + ordinal * self.stride,
            deleted: false,
        }
    }

    fn get(self, key: i64) -> Option<Entry> {
        let ordinal = u64::try_from(key.checked_sub(self.first_key)?).ok()?;
        (ordinal < self.count).then(|| self.entry(ordinal))
    }
}

pub(crate) enum StageIndex {
    Run { run: Run, fallback_path: PathBuf },
    Disk(DiskIndex),
}

impl StageIndex {
    pub(crate) fn new(path: PathBuf, entry: Entry, try_run: bool) -> Result<Self> {
        if try_run && !entry.deleted {
            return Ok(Self::Run {
                run: Run::new(entry),
                fallback_path: path,
            });
        }
        Ok(Self::Disk(Self::materialize(path, std::iter::once(entry))?))
    }

    fn materialize(path: PathBuf, entries: impl Iterator<Item = Entry>) -> Result<DiskIndex> {
        let mut disk = DiskIndex::create(path)?;
        for entry in entries {
            if let Err(error) = disk.put(entry) {
                let path = disk.path().to_owned();
                drop(disk);
                let _ = fs::remove_file(path);
                return Err(error);
            }
        }
        Ok(disk)
    }

    pub(crate) fn put(&mut self, entry: Entry) -> Result<()> {
        match self {
            Self::Run { run, fallback_path } => {
                if !run.append(entry) {
                    let old = *run;
                    let entries = (0..old.count).map(|ordinal| old.entry(ordinal));
                    let disk = Self::materialize(
                        fallback_path.clone(),
                        entries.chain(std::iter::once(entry)),
                    )?;
                    *self = Self::Disk(disk);
                }
                Ok(())
            }
            Self::Disk(disk) => disk.put(entry),
        }
    }

    pub(crate) fn get(&mut self, key: i64) -> Result<Option<Entry>> {
        match self {
            Self::Run { run, .. } => Ok(run.get(key)),
            Self::Disk(disk) => disk.get(key),
        }
    }

    pub(crate) fn reader(&self) -> Result<StageLookup> {
        match self {
            Self::Run { run, .. } => Ok(StageLookup::Run(*run)),
            Self::Disk(disk) => Ok(StageLookup::Disk(disk.reader()?)),
        }
    }

    pub(crate) fn entries(&self) -> Result<StageEntries> {
        match self {
            Self::Run { run, .. } => Ok(StageEntries::Run {
                run: *run,
                ordinals: 0..run.count,
            }),
            Self::Disk(disk) => Ok(StageEntries::Disk(disk.entries()?)),
        }
    }

    pub(crate) fn legacy_entries(&self, path: PathBuf) -> Result<StageEntries> {
        match self {
            Self::Run { run, .. } => {
                let disk =
                    Self::materialize(path, (0..run.count).map(|ordinal| run.entry(ordinal)))?;
                Ok(StageEntries::Disk(disk.entries()?))
            }
            Self::Disk(disk) => Ok(StageEntries::Disk(disk.entries()?)),
        }
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::Run { .. } => None,
            Self::Disk(disk) => Some(disk.path()),
        }
    }
}

pub(crate) enum StageLookup {
    Run(Run),
    Disk(DiskIndex),
}

impl StageLookup {
    pub(crate) fn get(&mut self, key: i64) -> Result<Option<Entry>> {
        match self {
            Self::Run(run) => Ok(run.get(key)),
            Self::Disk(disk) => disk.get(key),
        }
    }
}

pub(crate) enum StageEntries {
    Run { run: Run, ordinals: Range<u64> },
    Disk(Entries),
}

impl Iterator for StageEntries {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Run { run, ordinals } => ordinals.next().map(|ordinal| Ok(run.entry(ordinal))),
            Self::Disk(entries) => entries.next(),
        }
    }
}
