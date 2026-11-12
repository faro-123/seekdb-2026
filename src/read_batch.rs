//! Query-local ordering and buffered reads of immutable committed WAL positions.
use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};

use crate::Result;
use crate::committed_cache::CommittedView;
use crate::disk_index::Entry;
use crate::stage::StageRead;

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

struct ReadLimit<R> {
    source: R,
    limit: usize,
}

impl<R: Read> Read for ReadLimit<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let count = out.len().min(self.limit);
        self.source.read(&mut out[..count])
    }
}

impl<R: Seek> Seek for ReadLimit<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.source.seek(from)
    }
}

pub(crate) struct BufferedFile<R> {
    source: BufReader<ReadLimit<R>>,
    position: u64,
    prefetch: bool,
}

impl<R: Read + Seek> BufferedFile<R> {
    fn new(source: R) -> Self {
        Self {
            source: BufReader::with_capacity(
                READ_BYTES,
                ReadLimit {
                    source,
                    limit: usize::MAX,
                },
            ),
            position: 0,
            prefetch: true,
        }
    }
}

impl<R: Read> Read for BufferedFile<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.source.get_mut().limit = if self.prefetch { usize::MAX } else { out.len() };
        let count = self.source.read(out)?;
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
    Cached(CachedFile),
    Buffered(BufferedFile<File>),
    Stage(StageRead),
}

impl WalRead {
    pub fn cached(file: File, view: CommittedView) -> Self {
        Self::Cached(CachedFile {
            prefix: Cursor::new(view),
            tail: file,
        })
    }
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

pub(crate) struct CachedFile {
    prefix: Cursor<CommittedView>,
    tail: File,
}

impl Read for CachedFile {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let position = self.prefix.position();
        if position < self.prefix.get_ref().bytes().len() as u64 {
            return self.prefix.read(out);
        }
        self.tail.seek(SeekFrom::Start(position))?;
        let count = self.tail.read(out)?;
        self.prefix.set_position(position + count as u64);
        Ok(count)
    }
}

impl Seek for CachedFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        if let SeekFrom::End(_) = from {
            let position = self.tail.seek(from)?;
            self.prefix.set_position(position);
            Ok(position)
        } else {
            self.prefix.seek(from)
        }
    }
}

impl Read for WalRead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Direct(file) => file.read(out),
            Self::Cached(reader) => reader.read(out),
            Self::Buffered(reader) => reader.read(out),
            Self::Stage(reader) => reader.read(out),
        }
    }
}

impl Seek for WalRead {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Direct(file) => file.seek(from),
            Self::Cached(reader) => reader.seek(from),
            Self::Buffered(reader) => reader.seek(from),
            Self::Stage(reader) => reader.seek(from),
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
    fn cached_reader_combines_validated_prefix_and_new_file_tail() {
        use crate::committed_cache::{CommittedCache, MAX_BYTES};
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "mv-cached-read-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.write_all(b"prefix").unwrap();
        let cache = CommittedCache::refresh(None, &file, 6, MAX_BYTES)
            .unwrap()
            .unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"-tail").unwrap();
        let mut reader = WalRead::cached(File::open(&path).unwrap(), CommittedCache::view(&cache));
        let mut bytes = [0; 8];
        reader.seek(SeekFrom::Start(3)).unwrap();
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"fix-tail");
        reader.seek(SeekFrom::End(-4)).unwrap();
        assert_eq!(reader.read(&mut bytes).unwrap(), 4);
        assert_eq!(&bytes[..4], b"tail");
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut prefix = [0; 6];
        reader.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"prefix");
        drop((reader, file));
        std::fs::remove_file(path).unwrap();
    }

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

    #[test]
    fn backward_seek_after_sparse_read_uses_the_logical_position() {
        let bytes: Vec<_> = (0..24000).map(|n| (n % 251) as u8).collect();
        let mut reader = BufferedFile::new(Cursor::new(&bytes));
        let mut first = [0; 4];
        reader.read_exact(&mut first).unwrap();
        reader.seek(SeekFrom::Start(8192)).unwrap();
        reader.prefetch = false;
        let mut sparse = [0; 16];
        reader.read_exact(&mut sparse).unwrap();
        assert_eq!(sparse, bytes[8192..8208]);
        reader.seek(SeekFrom::Start(8190)).unwrap();
        let mut actual = [0; 8];
        reader.read_exact(&mut actual).unwrap();
        assert_eq!(actual, bytes[8190..8198]);
    }
}
