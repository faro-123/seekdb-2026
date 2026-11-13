//! Bounded resident positions for dense integer keys, with a disk hash fallback.
use std::cell::{Cell, RefCell, RefMut};
use std::path::PathBuf;
use std::rc::Rc;

use crate::Result;
use crate::disk_index::{DiskIndex, Entries, Entry};
use crate::stage::{Budget, Reservation};

const PAGE_SLOTS: usize = 512;
const BITMAP_WORDS: usize = PAGE_SLOTS / u64::BITS as usize;
const MAX_PAGES: usize = 64;
const MAX_PAGE_RUNS: usize = 8;
const MAX_SPAN_PAGES: u64 = 128;
// Admit a few pages before using occupied slots to distinguish a dense run
// from isolated keys. Use the span as well so extreme keys spill immediately.
const WARMUP_PAGES: usize = 4;
const MIN_OCCUPIED_PER_PAGE: usize = 12;
const ALLOCATION_ALLOWANCE: usize = 64;
const OWNER_BYTES: usize = size_of::<Resident>() + ALLOCATION_ALLOWANCE;
const PAGE_BYTES: usize = size_of::<Page>() + ALLOCATION_ALLOWANCE;
const DENSE_BYTES: usize = size_of::<[u64; PAGE_SLOTS]>() + ALLOCATION_ALLOWANCE;

#[derive(Clone)]
pub(crate) struct PositionBudget(Rc<BudgetState>);

struct BudgetState {
    cap: usize,
    used: Cell<usize>,
    stage: RefCell<Option<Reservation>>,
}

impl PositionBudget {
    pub fn new(stage: Budget, cap: usize) -> Self {
        // Reserve the entire ceiling before staging or replay starts. Adding
        // resident pages later cannot consume room reserved for block readers.
        let reservation = (cap != 0).then(|| stage.reserve(cap)).flatten();
        let cap = if reservation.is_some() { cap } else { 0 };
        Self(Rc::new(BudgetState {
            cap,
            used: Cell::new(0),
            stage: RefCell::new(reservation),
        }))
    }

    fn reserve(&self, bytes: usize) -> Option<Charge> {
        // An encoder can borrow unused headroom before replay, when it
        // neither allocates nor mutates index pages.
        let _stage = self.0.stage.try_borrow().ok()?;
        let next = self.0.used.get().checked_add(bytes)?;
        if next > self.0.cap {
            return None;
        }
        self.0.used.set(next);
        Some(Charge {
            budget: self.clone(),
            bytes,
        })
    }

    pub fn lend_unused(&self) -> PositionLoan<'_> {
        let mut stage = self.0.stage.borrow_mut();
        if let Some(reservation) = stage.as_mut() {
            assert!(reservation.resize(self.0.used.get()));
        }
        PositionLoan {
            stage,
            cap: self.0.cap,
        }
    }
}

pub(crate) struct PositionLoan<'a> {
    stage: RefMut<'a, Option<Reservation>>,
    cap: usize,
}

impl Drop for PositionLoan<'_> {
    fn drop(&mut self) {
        if let Some(reservation) = self.stage.as_mut() {
            assert!(reservation.resize(self.cap));
        }
    }
}

struct Charge {
    budget: PositionBudget,
    bytes: usize,
}

impl Drop for Charge {
    fn drop(&mut self) {
        let usage = &self.budget.0.used;
        usage.set(usage.get() - self.bytes);
    }
}

#[derive(Clone, Copy, Default)]
struct Run {
    start: u64,
    stride: u64,
    first: u16,
    end: u16,
}

impl Run {
    fn append(&mut self, slot: u16, offset: u64) -> bool {
        if slot != self.end {
            return false;
        }
        let ordinal = u64::from(slot - self.first);
        let Some(delta) = offset.checked_sub(self.start) else {
            return false;
        };
        if ordinal == 1 {
            self.stride = delta;
        } else if self.stride.checked_mul(ordinal) != Some(delta) {
            return false;
        }
        self.end += 1;
        true
    }

    fn at(&self, slot: u16) -> Option<u64> {
        // The append check bounds both the multiplication and sum.
        (self.first..self.end)
            .contains(&slot)
            .then(|| self.start + u64::from(slot - self.first) * self.stride)
    }
}

struct Runs {
    items: [Run; MAX_PAGE_RUNS],
    len: usize,
}

impl Runs {
    fn append(&mut self, slot: u16, offset: u64) -> bool {
        if self.len != 0 {
            let last = &mut self.items[self.len - 1];
            if last.append(slot, offset) {
                return true;
            }
            if slot < last.end {
                return false;
            }
        }
        let Some(next) = self.items.get_mut(self.len) else {
            return false;
        };
        *next = Run {
            start: offset,
            stride: 0,
            first: slot,
            end: slot + 1,
        };
        self.len += 1;
        true
    }

    fn at(&self, slot: u16) -> Option<u64> {
        self.items[..self.len]
            .iter()
            .rev()
            .find_map(|run| run.at(slot))
    }
}

enum Offsets {
    Runs(Runs),
    Dense {
        values: Box<[u64; PAGE_SLOTS]>,
        _charge: Charge,
    },
}

struct Page {
    offsets: Offsets,
    occupied: [u64; BITMAP_WORDS],
    live: [u64; BITMAP_WORDS],
}

impl Page {
    fn new() -> Self {
        Self {
            offsets: Offsets::Runs(Runs {
                items: [Run::default(); MAX_PAGE_RUNS],
                len: 0,
            }),
            occupied: [0; BITMAP_WORDS],
            live: [0; BITMAP_WORDS],
        }
    }

    fn bit(slot: usize) -> (usize, u64) {
        (slot / 64, 1 << (slot % 64))
    }

    fn set(&mut self, slot: usize, entry: Entry, budget: &PositionBudget) -> Option<bool> {
        match &mut self.offsets {
            Offsets::Runs(runs) => {
                if !runs.append(slot as u16, entry.offset) {
                    let charge = budget.reserve(DENSE_BYTES)?;
                    let mut values = Box::new([0; PAGE_SLOTS]);
                    for run in &runs.items[..runs.len] {
                        for i in run.first..run.end {
                            values[usize::from(i)] = run.at(i).expect("slot within its run");
                        }
                    }
                    values[slot] = entry.offset;
                    self.offsets = Offsets::Dense {
                        values,
                        _charge: charge,
                    };
                }
            }
            Offsets::Dense { values, .. } => values[slot] = entry.offset,
        }
        let (word, bit) = Self::bit(slot);
        let inserted = self.occupied[word] & bit == 0;
        self.occupied[word] |= bit;
        if entry.deleted {
            self.live[word] &= !bit;
        } else {
            self.live[word] |= bit;
        }
        Some(inserted)
    }

    fn entry(&self, number: i64, slot: usize) -> Entry {
        let (word, bit) = Self::bit(slot);
        Entry {
            // An i64 key divided by 512 with Euclidean remainder always
            // reconstructs within i64, including both extreme keys.
            key: number * PAGE_SLOTS as i64 + slot as i64,
            offset: match &self.offsets {
                Offsets::Runs(runs) => runs.at(slot as u16).expect("occupied slot in a run"),
                Offsets::Dense { values, .. } => values[slot],
            },
            deleted: self.live[word] & bit == 0,
        }
    }

    fn get(&self, number: i64, slot: usize) -> Option<Entry> {
        let (word, bit) = Self::bit(slot);
        (self.occupied[word] & bit != 0).then(|| self.entry(number, slot))
    }
}

struct PageSlot {
    number: i64,
    data: Box<Page>,
    _charge: Charge,
}

struct Resident {
    // Pages grow independently. A fixed sorted directory keeps descriptors
    // charged from creation and avoids reallocating an existing directory.
    pages: [Option<PageSlot>; MAX_PAGES],
    page_count: usize,
    occupied_count: usize,
    increasing_offsets: bool,
    last: Option<(i64, u64)>,
    _charge: Charge,
}

impl Resident {
    fn new(budget: &PositionBudget, path_capacity: usize) -> Option<Rc<Self>> {
        let charge = budget.reserve(OWNER_BYTES.checked_add(path_capacity)?)?;
        Some(Rc::new(Self {
            pages: std::array::from_fn(|_| None),
            page_count: 0,
            occupied_count: 0,
            increasing_offsets: true,
            last: None,
            _charge: charge,
        }))
    }

    fn split(key: i64) -> (i64, usize) {
        (
            key.div_euclid(PAGE_SLOTS as i64),
            key.rem_euclid(PAGE_SLOTS as i64) as usize,
        )
    }

    fn page(&self, index: usize) -> &PageSlot {
        self.pages[index]
            .as_ref()
            .expect("initialized prefix of position directory")
    }

    fn locate(&self, number: i64) -> std::result::Result<usize, usize> {
        self.pages[..self.page_count].binary_search_by_key(&number, |slot| {
            slot.as_ref()
                .expect("initialized prefix of position directory")
                .number
        })
    }

    fn get(&self, key: i64) -> Option<Entry> {
        let (number, slot) = Self::split(key);
        let page = self.locate(number).ok()?;
        self.page(page).data.get(number, slot)
    }

    fn admit(&self, number: i64) -> bool {
        if self.page_count == 0 {
            return true;
        }
        if self.page_count == MAX_PAGES {
            return false;
        }
        let low = self.page(0).number.min(number);
        let high = self.page(self.page_count - 1).number.max(number);
        high.abs_diff(low) < MAX_SPAN_PAGES
            && (self.page_count < WARMUP_PAGES
                || self.occupied_count >= self.page_count * MIN_OCCUPIED_PER_PAGE)
    }

    fn put(&mut self, entry: Entry, budget: &PositionBudget) -> bool {
        let (number, slot) = Self::split(entry.key);
        let index = match self.locate(number) {
            Ok(index) => index,
            Err(index) => {
                if !self.admit(number) {
                    return false;
                }
                let Some(charge) = budget.reserve(PAGE_BYTES) else {
                    return false;
                };
                let new = PageSlot {
                    number,
                    data: Box::new(Page::new()),
                    _charge: charge,
                };
                for old in (index..self.page_count).rev() {
                    self.pages[old + 1] = self.pages[old].take();
                }
                self.pages[index] = Some(new);
                self.page_count += 1;
                index
            }
        };
        let page = self.pages[index]
            .as_mut()
            .expect("initialized position page");
        let Some(inserted) = page.data.set(slot, entry, budget) else {
            return false;
        };
        self.occupied_count += usize::from(inserted);
        self.increasing_offsets &= inserted
            && !entry.deleted
            && self
                .last
                .is_none_or(|(key, offset)| key < entry.key && offset < entry.offset);
        self.last = Some((entry.key, entry.offset));
        true
    }
}

enum Backend {
    Resident { path: PathBuf, rows: Rc<Resident> },
    Disk(DiskIndex),
}

pub(crate) struct PositionIndex {
    backend: Backend,
    budget: PositionBudget,
}

impl PositionIndex {
    pub fn create(path: PathBuf, budget: PositionBudget) -> Result<Self> {
        let backend = if let Some(rows) = Resident::new(&budget, path.capacity()) {
            Backend::Resident { path, rows }
        } else {
            Backend::Disk(DiskIndex::create(path)?)
        };
        Ok(Self { backend, budget })
    }

    pub fn get(&mut self, key: i64) -> Result<Option<Entry>> {
        match &mut self.backend {
            Backend::Resident { rows, .. } => Ok(rows.get(key)),
            Backend::Disk(disk) => disk.get(key),
        }
    }

    pub fn put(&mut self, entry: Entry) -> Result<()> {
        let (path, rows) = match &mut self.backend {
            Backend::Disk(disk) => return disk.put(entry),
            Backend::Resident { path, rows } => (path, rows),
        };
        if let Some(resident) = Rc::get_mut(rows)
            && resident.put(entry, &self.budget)
        {
            return Ok(());
        }

        // An existing reader can retain this complete generation. If we
        // cannot extend it in place, build the replacement before publishing
        // it, including the entry that triggered the transition.
        let mut disk = DiskIndex::create(path.clone())?;
        let copy = (|| {
            for existing in ResidentEntries::new(Rc::clone(rows)) {
                disk.put(existing)?;
            }
            disk.put(entry)
        })();
        if let Err(error) = copy {
            let attempted = disk.path().to_owned();
            drop(disk);
            let _ = std::fs::remove_file(attempted);
            return Err(error);
        }
        self.backend = Backend::Disk(disk);
        Ok(())
    }

    pub fn reader(&self) -> Result<PositionReader> {
        let inner = match &self.backend {
            Backend::Resident { rows, .. } => Reader::Resident(Rc::clone(rows)),
            Backend::Disk(disk) => Reader::Disk(disk.reader()?),
        };
        Ok(PositionReader { inner })
    }

    pub fn entries(&self) -> Result<PositionEntries> {
        let inner = match &self.backend {
            Backend::Resident { rows, .. } => Iter::Resident(ResidentEntries::new(Rc::clone(rows))),
            Backend::Disk(disk) => Iter::Disk(disk.entries()?),
        };
        Ok(PositionEntries { inner })
    }
}

enum Reader {
    Resident(Rc<Resident>),
    Disk(DiskIndex),
}

pub(crate) struct PositionReader {
    inner: Reader,
}

impl PositionReader {
    pub fn get(&mut self, key: i64) -> Result<Option<Entry>> {
        match &mut self.inner {
            Reader::Resident(rows) => Ok(rows.get(key)),
            Reader::Disk(disk) => disk.get(key),
        }
    }
}

struct ResidentEntries {
    rows: Rc<Resident>,
    page: usize,
    word: usize,
    remaining: u64,
}

impl ResidentEntries {
    fn new(rows: Rc<Resident>) -> Self {
        Self {
            rows,
            page: 0,
            word: 0,
            remaining: 0,
        }
    }
}

impl Iterator for ResidentEntries {
    type Item = Entry;

    fn next(&mut self) -> Option<Self::Item> {
        while self.page < self.rows.page_count {
            let page = self.rows.page(self.page);
            if self.remaining != 0 {
                let bit = self.remaining.trailing_zeros() as usize;
                self.remaining &= self.remaining - 1;
                return Some(page.data.entry(page.number, (self.word - 1) * 64 + bit));
            }
            if self.word == BITMAP_WORDS {
                self.page += 1;
                self.word = 0;
            } else {
                // Primary readers must see tombstone keys so a staged insert
                // can replace one without disappearing from the overlay view.
                self.remaining = page.data.occupied[self.word];
                self.word += 1;
            }
        }
        None
    }
}

enum Iter {
    Resident(ResidentEntries),
    Disk(Entries),
}

pub(crate) struct PositionEntries {
    inner: Iter,
}

impl PositionEntries {
    pub fn increasing_offsets(&self) -> bool {
        matches!(&self.inner, Iter::Resident(rows) if rows.rows.increasing_offsets)
    }
}

impl Iterator for PositionEntries {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            Iter::Resident(rows) => rows.next().map(Ok),
            Iter::Disk(rows) => rows.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mv-position-{}-{id}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(key: i64, offset: u64, deleted: bool) -> Entry {
        Entry {
            key,
            offset,
            deleted,
        }
    }

    fn rows(index: &PositionIndex) -> Vec<(i64, u64, bool)> {
        let mut rows: Vec<_> = index
            .entries()
            .unwrap()
            .map(|result| {
                let row = result.unwrap();
                (row.key, row.offset, row.deleted)
            })
            .collect();
        rows.sort_unstable();
        rows
    }

    #[test]
    fn consecutive_block_positions_use_bounded_resident_pages() {
        let dir = TempDir::new();
        let budget = PositionBudget::new(Budget::default(), 192 << 10);
        let mut primary = PositionIndex::create(dir.0.join("primary"), budget.clone()).unwrap();
        let mut vectors = PositionIndex::create(dir.0.join("vectors"), budget.clone()).unwrap();
        for key in 0..2048 {
            let block = u64::try_from(key / 100).unwrap();
            let ordinal = u64::try_from(key % 100).unwrap();
            primary
                .put(entry(key, (1 << 63) + (block << 28) + ordinal, false))
                .unwrap();
            vectors
                .put(entry(
                    key,
                    block * 32768 + ordinal * (195 + block % 3),
                    false,
                ))
                .unwrap();
        }
        assert!(
            budget.0.used.get() < 12 << 10,
            "used {} bytes",
            budget.0.used.get()
        );
        for key in 0..2048 {
            let block = u64::try_from(key / 100).unwrap();
            let ordinal = u64::try_from(key % 100).unwrap();
            assert_eq!(
                primary.get(key).unwrap().unwrap().offset,
                (1 << 63) + (block << 28) + ordinal
            );
            assert_eq!(
                vectors.get(key).unwrap().unwrap().offset,
                block * 32768 + ordinal * (195 + block % 3)
            );
        }
        assert!(primary.entries().unwrap().increasing_offsets());
        assert!(vectors.entries().unwrap().increasing_offsets());
        let mut reader = vectors.reader().unwrap();
        assert_eq!(reader.get(7).unwrap().unwrap().offset, 7 * 195);
        drop(reader);
        vectors.put(entry(7, u64::MAX, false)).unwrap();
        assert!(!vectors.entries().unwrap().increasing_offsets());
        assert_eq!(vectors.get(7).unwrap().unwrap().offset, u64::MAX);
        assert_eq!(vectors.get(8).unwrap().unwrap().offset, 8 * 195);
        assert_eq!(primary.entries().unwrap().count(), 2048);
        assert_eq!(vectors.entries().unwrap().count(), 2048);
    }

    #[test]
    fn preserves_signed_page_boundaries_and_full_offsets() {
        let dir = TempDir::new();
        let budget = PositionBudget::new(Budget::default(), 192 << 10);
        let mut index = PositionIndex::create(dir.0.join("slots"), budget).unwrap();
        for row in [
            entry(-513, 0, false),
            entry(-512, 1 << 63, false),
            entry(-1, u64::MAX, true),
            entry(0, 2, false),
            entry(511, 7, false),
            entry(512, 8, false),
        ] {
            index.put(row).unwrap();
        }
        assert!(matches!(&index.backend, Backend::Resident { .. }));
        assert_eq!(
            rows(&index),
            [
                (-513, 0, false),
                (-512, 1 << 63, false),
                (-1, u64::MAX, true),
                (0, 2, false),
                (511, 7, false),
                (512, 8, false),
            ]
        );
        let mut reader = index.reader().unwrap();
        assert!(reader.get(-500).unwrap().is_none());
        assert!(reader.get(-1).unwrap().unwrap().deleted);
    }

    #[test]
    fn extreme_key_spills_without_losing_tombstones_or_prior_readers() {
        let dir = TempDir::new();
        let budget = PositionBudget::new(Budget::default(), 192 << 10);
        let mut index = PositionIndex::create(dir.0.join("slots"), budget.clone()).unwrap();
        index.put(entry(i64::MIN, 0, false)).unwrap();
        index.put(entry(i64::MIN + 511, 1 << 63, true)).unwrap();
        let mut prior = index.reader().unwrap();
        let resident = budget.0.used.get();
        index.put(entry(i64::MAX, u64::MAX, false)).unwrap();
        assert!(matches!(&index.backend, Backend::Disk(_)));
        assert_eq!(
            rows(&index),
            [
                (i64::MIN, 0, false),
                (i64::MIN + 511, 1 << 63, true),
                (i64::MAX, u64::MAX, false)
            ]
        );
        assert!(prior.get(i64::MAX).unwrap().is_none());
        assert_eq!(budget.0.used.get(), resident);
        drop(prior);
        assert_eq!(budget.0.used.get(), 0);
    }

    #[test]
    fn failed_disk_spill_keeps_the_resident_state() {
        let dir = TempDir::new();
        let path = dir.0.join("missing").join("slots");
        let budget = PositionBudget::new(Budget::default(), 192 << 10);
        let mut index = PositionIndex::create(path, budget.clone()).unwrap();
        index.put(entry(0, 17, true)).unwrap();
        let before = budget.0.used.get();
        assert!(index.put(entry(i64::MAX, 83, false)).is_err());
        assert_eq!(rows(&index), [(0, 17, true)]);
        assert_eq!(budget.0.used.get(), before);
    }

    #[test]
    fn shared_cap_counts_both_indexes_and_uses_one_stage_reservation() {
        let dir = TempDir::new();
        let stage = Budget::default();
        let row_path = dir.0.join("rows");
        let vector_path = dir.0.join("vectors");
        let cap = 2 * OWNER_BYTES + row_path.capacity() + vector_path.capacity() + PAGE_BYTES;
        let budget = PositionBudget::new(stage.clone(), cap);
        assert_eq!(budget.0.cap, cap);
        let room = stage.reserve((1024 - 112) * 1024 - cap).unwrap();
        assert!(stage.reserve(1).is_none());
        drop(room);

        let mut row = PositionIndex::create(row_path, budget.clone()).unwrap();
        let mut vector = PositionIndex::create(vector_path, budget.clone()).unwrap();
        row.put(entry(0, 11, false)).unwrap();
        vector.put(entry(0, 12, false)).unwrap();
        assert!(matches!(&row.backend, Backend::Resident { .. }));
        assert!(matches!(&vector.backend, Backend::Disk(_)));
        assert!(budget.0.used.get() <= cap);
        assert_eq!(row.get(0).unwrap().unwrap().offset, 11);
        assert_eq!(vector.get(0).unwrap().unwrap().offset, 12);
        let used = budget.0.used.get();
        let positions = budget.lend_unused();
        assert!(budget.reserve(1).is_none());
        let encode = stage.reserve((1024 - 112) * 1024 - used).unwrap();
        assert!(stage.reserve(1).is_none());
        drop(encode);
        drop(positions);
        assert!(stage.reserve((1024 - 112) * 1024 - cap + 1).is_none());
    }

    #[test]
    fn sparse_pages_fall_back_before_spending_the_whole_resident_cap() {
        let dir = TempDir::new();
        let budget = PositionBudget::new(Budget::default(), 192 << 10);
        let mut index = PositionIndex::create(dir.0.join("slots"), budget.clone()).unwrap();
        for i in 0..5 {
            index
                .put(entry(i * PAGE_SLOTS as i64, i as u64, false))
                .unwrap();
        }
        assert!(matches!(&index.backend, Backend::Disk(_)));
        assert_eq!(budget.0.used.get(), 0);
        assert_eq!(rows(&index).len(), 5);
    }
}
