//! Query-local ordering and buffered reads of immutable committed WAL positions.
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

use crate::Result;
use crate::disk_index::Entry;

const BATCH_ENTRIES: usize = 4096;
const READ_BYTES: usize = 8192;

pub(crate) struct OffsetBatch<I> {
    source: I,
    ordered: bool,
    batch: Vec<Entry>,
    cursor: usize,
}

impl<I> OffsetBatch<I> {
    pub fn new(source: I) -> Self {
        Self {
            source,
            ordered: false,
            batch: Vec::new(),
            cursor: 0,
        }
    }

    pub fn ordered(mut self) -> Self {
        self.ordered = true;
        self.batch = Vec::with_capacity(BATCH_ENTRIES);
        self
    }

    pub fn nearby(&self, offset: u64) -> bool {
        self.batch
            .get(self.cursor)
            .is_some_and(|next| next.offset >= offset && next.offset - offset < READ_BYTES as u64)
    }
}

impl<I: Iterator<Item = Result<Entry>>> Iterator for OffsetBatch<I> {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.ordered {
            return self.source.next();
        }
        if self.cursor == self.batch.len() {
            self.batch.clear();
            self.cursor = 0;
            for record in self.source.by_ref().take(BATCH_ENTRIES) {
                match record {
                    Ok(entry) => self.batch.push(entry),
                    Err(error) => {
                        self.batch.clear();
                        return Some(Err(error));
                    }
                }
            }
            if self.batch.is_empty() {
                return None;
            }
            self.batch.sort_unstable_by_key(|entry| entry.offset);
        }
        let entry = self.batch[self.cursor];
        self.cursor += 1;
        Some(Ok(entry))
    }
}

pub(crate) struct BufferedFile<R> {
    source: BufReader<R>,
    position: u64,
    prefetch: bool,
}

impl<R: Read + Seek> BufferedFile<R> {
    fn new(source: R) -> Self {
        Self {
            source: BufReader::with_capacity(READ_BYTES, source),
            position: 0,
            prefetch: true,
        }
    }
}

impl<R: Read> Read for BufferedFile<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let count = if !self.prefetch && self.source.buffer().is_empty() {
            // Sparse positions keep exact-sized reads instead of reading a whole window.
            self.source.get_mut().read(out)?
        } else {
            self.source.read(out)?
        };
        self.position += count as u64;
        Ok(count)
    }
}

impl<R: Read + Seek> Seek for BufferedFile<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(delta) => {
                self.position.checked_add_signed(delta).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "negative WAL offset")
                })?
            }
            SeekFrom::End(_) => {
                self.position = self.source.seek(from)?;
                return Ok(self.position);
            }
        };
        let delta = i128::from(target) - i128::from(self.position);
        if let Ok(delta) = i64::try_from(delta) {
            // seek_relative keeps the existing buffer when the position is covered.
            self.source.seek_relative(delta)?;
        } else {
            self.source.seek(SeekFrom::Start(target))?;
        }
        self.position = target;
        Ok(target)
    }
}

pub(crate) enum WalRead {
    Direct(File),
    Buffered(BufferedFile<File>),
}

impl WalRead {
    pub fn prefetch(&mut self, enabled: bool) {
        if let Self::Buffered(reader) = self {
            reader.prefetch = enabled;
        }
    }

    pub fn buffered(self) -> Self {
        match self {
            Self::Direct(file) => Self::Buffered(BufferedFile::new(file)),
            buffered => buffered,
        }
    }
}

impl Read for WalRead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Direct(file) => file.read(out),
            Self::Buffered(reader) => reader.read(out),
        }
    }
}

impl Seek for WalRead {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Direct(file) => file.seek(from),
            Self::Buffered(reader) => reader.seek(from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Cursor;
    use std::rc::Rc;

    #[test]
    fn reads_cross_buffer_boundaries_and_large_reads_after_backward_seeks() {
        let bytes: Vec<_> = (0..240_000).map(|n| (n % 251) as u8).collect();
        let mut reader = BufferedFile::new(Cursor::new(&bytes));
        for (offset, count) in [(8190, 12), (8188, 100), (16000, 196608), (8191, 2053 * 4)] {
            reader.seek(SeekFrom::Start(offset as u64)).unwrap();
            let mut actual = vec![0; count];
            reader.read_exact(&mut actual).unwrap();
            assert_eq!(actual, bytes[offset..offset + count]);
        }
        reader.prefetch = false;
        reader.seek(SeekFrom::Start(230000)).unwrap();
        let mut sparse = [0; 16];
        reader.read_exact(&mut sparse).unwrap();
        assert_eq!(sparse, bytes[230000..230016]);
        reader.prefetch = true;
        reader.seek(SeekFrom::End(-3)).unwrap();
        let mut last = [0; 3];
        reader.read_exact(&mut last).unwrap();
        assert_eq!(last, bytes[bytes.len() - 3..]);
        assert!(reader.seek(SeekFrom::Current(-300_000)).is_err());
    }

    #[test]
    fn offset_ordering_reads_only_one_bounded_batch_ahead() {
        let consumed = Rc::new(Cell::new(0));
        let counter = Rc::clone(&consumed);
        let entries = (0..9000).map(move |n| {
            counter.set(counter.get() + 1);
            Ok(Entry {
                key: n,
                offset: (9000 - n) as u64,
                deleted: false,
            })
        });
        let mut ordered = OffsetBatch::new(entries).ordered();
        assert_eq!(ordered.next().unwrap().unwrap().key, 4095);
        assert_eq!(consumed.get(), 4096);
        let remaining: Vec<_> = ordered.map(|r| r.unwrap().key).collect();
        assert_eq!(remaining.len(), 8999);
        assert_eq!(remaining[4095], 8191);
        assert_eq!(remaining.last(), Some(&8192));
    }

    #[test]
    fn sparse_positions_read_only_requested_bytes() {
        struct Counted {
            data: Cursor<Vec<u8>>,
            bytes: Rc<Cell<usize>>,
        }
        impl Read for Counted {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let count = self.data.read(out)?;
                self.bytes.set(self.bytes.get() + count);
                Ok(count)
            }
        }
        impl Seek for Counted {
            fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
                self.data.seek(from)
            }
        }
        let bytes = Rc::new(Cell::new(0));
        let mut reader = BufferedFile::new(Counted {
            data: Cursor::new(vec![7; 24000]),
            bytes: Rc::clone(&bytes),
        });
        reader.prefetch = false;
        reader.seek(SeekFrom::Start(12000)).unwrap();
        let mut value = [0; 4];
        reader.read_exact(&mut value).unwrap();
        assert_eq!(value, [7; 4]);
        assert_eq!(bytes.get(), 4);
        reader.prefetch = true;
        reader.seek(SeekFrom::Start(0)).unwrap();
        reader.read_exact(&mut value).unwrap();
        let filled = bytes.get();
        assert_eq!(filled, 4 + READ_BYTES);
        reader.seek(SeekFrom::Start(1000)).unwrap();
        reader.read_exact(&mut value).unwrap();
        assert_eq!(bytes.get(), filled);
    }
}
