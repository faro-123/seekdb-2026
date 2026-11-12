use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::rc::Rc;

use crate::{Error, Result};

const TOTAL_BUDGET: usize = 1 << 20;
// The commit copy buffer, merge reader, record writer, and fixed ownership
// metadata share this allowance. Encoded allocations and their Rc owners are
// charged separately, including while a reader holds an older allocation.
const FIXED_BUDGET: usize = 112 << 10;
// Compact replay may hold its record and expansion while building a vector
// index through a second reader. Stages leave this space available.
const DECODE_HEADROOM: usize = 384 << 10;
const MEMORY_OVERHEAD: usize = 64;
const MIN_ALLOCATION: usize = 4 << 10;
const FILE_WRITE_CHUNK: usize = 4 << 10;

#[derive(Clone, Default)]
pub(crate) struct Budget(Rc<Cell<usize>>);

impl Budget {
    fn available(&self) -> usize {
        TOTAL_BUDGET - FIXED_BUDGET - self.0.get()
    }

    fn stage_available(&self) -> usize {
        self.available().saturating_sub(DECODE_HEADROOM)
    }

    pub fn reserve(&self, bytes: usize) -> Option<Reservation> {
        if bytes > self.available() {
            return None;
        }
        self.0.set(self.0.get() + bytes);
        Some(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}

pub(crate) struct Reservation {
    budget: Budget,
    bytes: usize,
}

impl Reservation {
    pub fn resize(&mut self, bytes: usize) -> bool {
        let occupied = self.budget.0.get() - self.bytes;
        if bytes > TOTAL_BUDGET - FIXED_BUDGET - occupied {
            return false;
        }
        self.budget.0.set(occupied + bytes);
        self.bytes = bytes;
        true
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.0.set(self.budget.0.get() - self.bytes);
    }
}

struct Memory {
    data: Box<[u8]>,
    _reservation: Reservation,
    len: usize,
}

impl Memory {
    fn new(capacity: usize, budget: Budget) -> Self {
        let reservation = budget
            .reserve(capacity + MEMORY_OVERHEAD)
            .expect("staging space was checked");
        Self {
            data: vec![0; capacity].into_boxed_slice(),
            _reservation: reservation,
            len: 0,
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

pub(crate) struct StageBytes(Option<Rc<Memory>>);

impl AsRef<[u8]> for StageBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_deref().map_or(&[], Memory::bytes)
    }
}

pub(crate) enum StageRead {
    Memory(Cursor<StageBytes>),
    File(File),
}

impl Read for StageRead {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Memory(reader) => reader.read(buf),
            Self::File(reader) => reader.read(buf),
        }
    }
}

impl Seek for StageRead {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Memory(reader) => reader.seek(position),
            Self::File(reader) => reader.seek(position),
        }
    }
}

pub(crate) enum RecordTarget<'a> {
    Memory(&'a mut [u8]),
    File(BufWriter<&'a mut File>, usize),
}

impl Write for RecordTarget<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Memory(target) => target.write(bytes),
            Self::File(target, remaining) => {
                let n = target.write(&bytes[..bytes.len().min(*remaining)])?;
                *remaining -= n;
                Ok(n)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Memory(_) => Ok(()),
            Self::File(target, _) => target.flush(),
        }
    }
}

enum Backend {
    Empty,
    Memory(Rc<Memory>),
    File(File),
}

pub(crate) struct Stage {
    budget: Budget,
    path: PathBuf,
    backend: Backend,
    len: u64,
}

impl Stage {
    pub fn new(path: PathBuf, budget: Budget) -> Self {
        Self {
            budget,
            path,
            backend: Backend::Empty,
            len: 0,
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn bytes(&self) -> Option<&[u8]> {
        match &self.backend {
            Backend::Empty => Some(&[]),
            Backend::Memory(memory) => Some(memory.bytes()),
            Backend::File(_) => None,
        }
    }

    pub fn reader(&self) -> io::Result<StageRead> {
        Ok(match &self.backend {
            Backend::Empty => StageRead::Memory(Cursor::new(StageBytes(None))),
            Backend::Memory(memory) => {
                StageRead::Memory(Cursor::new(StageBytes(Some(Rc::clone(memory)))))
            }
            // File::try_clone shares the file position with the writer.
            Backend::File(_) => StageRead::File(File::open(&self.path)?),
        })
    }

    fn fit_memory(&mut self, end: usize) -> bool {
        let previous = match &mut self.backend {
            Backend::Empty => None,
            Backend::File(_) => return false,
            Backend::Memory(memory) => {
                let Some(memory) = Rc::get_mut(memory) else {
                    return false;
                };
                if end <= memory.data.len() {
                    return true;
                }
                Some(memory)
            }
        };
        let available = self
            .budget
            .stage_available()
            .saturating_sub(MEMORY_OVERHEAD);
        if end > available {
            return false;
        }
        // Reserve for both allocations before copying. On contention try the
        // exact size, then spill if the old and new buffers cannot coexist.
        let target = end.next_power_of_two().max(MIN_ALLOCATION);
        let capacity = if target <= available { target } else { end };
        let mut memory = Memory::new(capacity, self.budget.clone());
        if let Some(previous) = previous {
            memory.data[..previous.len].copy_from_slice(previous.bytes());
            memory.len = previous.len;
        }
        self.backend = Backend::Memory(Rc::new(memory));
        true
    }

    fn spill(&mut self) -> io::Result<()> {
        if matches!(self.backend, Backend::File(_)) {
            return Ok(());
        }
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&self.path)?;
        let previous = std::mem::replace(&mut self.backend, Backend::File(file));
        if let (Backend::Memory(memory), Backend::File(file)) = (previous, &mut self.backend) {
            file.write_all(memory.bytes())?;
        }
        Ok(())
    }

    pub fn append(
        &mut self,
        size: usize,
        emit: impl FnOnce(&mut RecordTarget<'_>) -> Result<()>,
    ) -> Result<u64> {
        let start = self.len;
        let end = start
            .checked_add(size as u64)
            .ok_or_else(|| Error::new("resource_limit", "stage exceeds format limit"))?;
        if !usize::try_from(end).is_ok_and(|end| self.fit_memory(end)) {
            self.spill()?;
        }
        match &mut self.backend {
            Backend::Memory(memory) => {
                let memory = Rc::get_mut(memory).expect("staging allocation is uniquely owned");
                let end = memory.len + size;
                let mut target = RecordTarget::Memory(&mut memory.data[memory.len..end]);
                emit(&mut target)?;
                if !matches!(target, RecordTarget::Memory(remaining) if remaining.is_empty()) {
                    return Err(Error::new(
                        "storage_error",
                        "encoded record length mismatch",
                    ));
                }
                memory.len = end;
            }
            Backend::File(file) => {
                let mut target =
                    RecordTarget::File(BufWriter::with_capacity(FILE_WRITE_CHUNK, file), size);
                emit(&mut target)?;
                if !matches!(target, RecordTarget::File(_, 0)) {
                    return Err(Error::new(
                        "storage_error",
                        "encoded record length mismatch",
                    ));
                }
                target.flush()?;
            }
            Backend::Empty => unreachable!("append selects a backing store"),
        }
        self.len = end;
        Ok(start)
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Backend::File(file) = std::mem::replace(&mut self.backend, Backend::Empty) {
            drop(file);
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_counts_pinned_allocations_and_does_not_remove_foreign_files() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("mv-stage-budget-{unique}-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        let budget = Budget::default();
        let capacity = budget.stage_available() - MEMORY_OVERHEAD;
        let mut full = Stage::new(path.join("full"), budget.clone());
        let fill = |n: usize, target: &mut RecordTarget<'_>| {
            let bytes = [9; 4096];
            for _ in 0..n / bytes.len() {
                target.write_all(&bytes)?;
            }
            target.write_all(&bytes[..n % bytes.len()])?;
            Ok(())
        };
        assert_eq!(full.append(capacity, |out| fill(capacity, out)).unwrap(), 0);
        assert_eq!(full.bytes().unwrap().len(), capacity);
        assert_eq!(budget.available(), DECODE_HEADROOM);
        let mut reader = full.reader().unwrap();
        drop(full);
        assert_eq!(budget.available(), DECODE_HEADROOM);

        let decoder = budget.reserve(DECODE_HEADROOM).unwrap();
        assert!(budget.reserve(1).is_none());
        drop(decoder);

        let blocked_path = path.join("blocked");
        fs::write(&blocked_path, b"owned elsewhere").unwrap();
        let mut blocked = Stage::new(blocked_path.clone(), budget.clone());
        let error = blocked.append(1, |out| fill(1, out)).unwrap_err();
        assert_eq!(error.code, "io_error");
        assert_eq!(blocked.len(), 0);
        drop(blocked);
        assert_eq!(fs::read(blocked_path).unwrap(), b"owned elsewhere");

        let mut tail = [0; 1];
        reader.seek(SeekFrom::End(-1)).unwrap();
        reader.read_exact(&mut tail).unwrap();
        assert_eq!(tail, [9]);
        drop(reader);
        assert_eq!(budget.available(), TOTAL_BUDGET - FIXED_BUDGET);

        let mut oversized = Stage::new(path.join("oversized"), budget.clone());
        let size = capacity + 1;
        assert_eq!(oversized.append(size, |out| fill(size, out)).unwrap(), 0);
        assert!(oversized.bytes().is_none());
        assert_eq!(oversized.len(), size as u64);
        assert_eq!(
            fs::metadata(path.join("oversized")).unwrap().len(),
            size as u64
        );
        assert_eq!(budget.available(), TOTAL_BUDGET - FIXED_BUDGET);
        drop(oversized);
        fs::remove_dir_all(path).unwrap();
    }
}
