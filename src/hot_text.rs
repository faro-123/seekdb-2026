//! Bounded text columns in a hot snapshot.
use std::cmp::Ordering;

const SAMPLE_LIMIT: usize = 4096;
const MIN_AFFIX: usize = 8;

#[derive(Default)]
pub(crate) struct TextShape {
    sample: Vec<u8>,
    prefix: usize,
    suffix: usize,
    minimum: usize,
    width: Option<usize>,
    observed: bool,
}

impl TextShape {
    pub fn observe(&mut self, text: &[u8]) {
        if !self.observed {
            self.observed = true;
            self.width = Some(text.len());
            self.minimum = text.len();
            if (MIN_AFFIX..=SAMPLE_LIMIT).contains(&text.len()) {
                self.sample.extend_from_slice(text);
                self.prefix = text.len();
                self.suffix = text.len();
            }
            return;
        }
        if self.width != Some(text.len()) {
            self.width = None;
        }
        self.minimum = self.minimum.min(text.len());
        self.prefix = self.sample[..self.prefix]
            .iter()
            .zip(text)
            .take_while(|(a, b)| a == b)
            .count();
        self.suffix = self.sample[self.sample.len() - self.suffix..]
            .iter()
            .rev()
            .zip(text.iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
    }

    pub fn allocate(self, rows: usize, raw_bytes: usize) -> HotText {
        let mut prefix = self.prefix;
        let mut suffix = self.suffix.min(self.minimum - prefix);
        if rows < 2 || prefix + suffix < MIN_AFFIX {
            prefix = 0;
            suffix = 0;
        }
        let affix_len = prefix + suffix;
        let stride = self.width.map(|width| width - affix_len);
        let mut offsets = Vec::new();
        if stride.is_none() {
            offsets = Vec::with_capacity(rows + 1);
            offsets.push(0);
        }
        HotText {
            prefix: self.sample[..prefix].into(),
            suffix: self.sample[self.sample.len() - suffix..].into(),
            offsets,
            stride,
            bytes: Vec::with_capacity(raw_bytes - rows * affix_len),
        }
    }
}

pub(crate) struct HotText {
    prefix: Box<[u8]>,
    suffix: Box<[u8]>,
    offsets: Vec<u32>,
    stride: Option<usize>,
    bytes: Vec<u8>,
}

impl HotText {
    pub fn push(&mut self, slot: usize, text: &[u8]) -> bool {
        let Some(middle) = text
            .strip_prefix(self.prefix.as_ref())
            .and_then(|tail| tail.strip_suffix(self.suffix.as_ref()))
        else {
            return false;
        };
        let end = self.bytes.len() + middle.len();
        if end > self.bytes.capacity() {
            return false;
        }
        if let Some(width) = self.stride {
            if middle.len() != width || self.bytes.len() != slot * width {
                return false;
            }
        } else {
            if self.offsets.len() != slot + 1 || self.offsets.len() == self.offsets.capacity() {
                return false;
            }
            let Ok(end) = u32::try_from(end) else {
                return false;
            };
            self.offsets.push(end);
        }
        self.bytes.extend_from_slice(middle);
        true
    }

    fn middle(&self, slot: usize) -> &[u8] {
        if let Some(width) = self.stride {
            &self.bytes[slot * width..][..width]
        } else {
            &self.bytes[self.offsets[slot] as usize..self.offsets[slot + 1] as usize]
        }
    }

    pub fn value<'a>(&'a self, slot: usize, scratch: &'a mut Vec<u8>) -> &'a [u8] {
        let middle = self.middle(slot);
        if self.prefix.is_empty() && self.suffix.is_empty() {
            return middle;
        }
        scratch.clear();
        scratch.extend_from_slice(&self.prefix);
        scratch.extend_from_slice(middle);
        scratch.extend_from_slice(&self.suffix);
        scratch
    }

    pub fn compare_slots(&self, left: usize, right: usize) -> Ordering {
        // Both values have the same prefix and suffix. With fixed lengths,
        // comparing the middles also gives the original byte ordering.
        if self.stride.is_some() || self.suffix.is_empty() {
            return self.middle(left).cmp(self.middle(right));
        }
        self.middle(left)
            .iter()
            .chain(self.suffix.iter())
            .cmp(self.middle(right).iter().chain(self.suffix.iter()))
    }

    pub fn compare(&self, slot: usize, mut wanted: &[u8]) -> Ordering {
        let middle = self.middle(slot);
        if self.prefix.is_empty() && self.suffix.is_empty() {
            return middle.cmp(wanted);
        }
        for part in [self.prefix.as_ref(), middle, self.suffix.as_ref()] {
            let n = part.len().min(wanted.len());
            let cmp = part[..n].cmp(&wanted[..n]);
            if cmp != Ordering::Equal {
                return cmp;
            }
            if n < part.len() {
                return Ordering::Greater;
            }
            wanted = &wanted[n..];
        }
        0.cmp(&wanted.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_text_preserves_bytes_and_order_across_utf8_boundaries() {
        for rows in [
            [
                "共同前缀甲共同后缀",
                "共同前缀乙共同后缀",
                "共同前缀丙共同后缀",
            ],
            [
                "共同前缀甲共同后缀",
                "共同前缀乙乙共同后缀",
                "共同前缀共同后缀",
            ],
            ["", "甲", "甲乙"],
        ] {
            let mut shape = TextShape::default();
            for value in rows {
                shape.observe(value.as_bytes());
            }
            let mut text = shape.allocate(rows.len(), rows.iter().map(|v| v.len()).sum());
            for (slot, value) in rows.iter().enumerate() {
                assert!(text.push(slot, value.as_bytes()));
            }
            let mut scratch = Vec::new();
            for (slot, value) in rows.iter().enumerate() {
                assert_eq!(text.value(slot, &mut scratch), value.as_bytes());
                for wanted in
                    rows.into_iter()
                        .chain(["", "共同前缀", "高", "共同前缀甲共同后缀extra"])
                {
                    assert_eq!(
                        text.compare(slot, wanted.as_bytes()),
                        value.as_bytes().cmp(wanted.as_bytes())
                    );
                }
            }
        }
    }
}
