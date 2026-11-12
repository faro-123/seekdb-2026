//! Exact AVX2 distance over an original lossless WAL vector field.
use crate::vector_codec::{HEADER_BYTES, Header as VectorHeader};

const FIELD_HEADER_BYTES: usize = 5 + HEADER_BYTES;
const MAX_COEFFICIENT: i32 = 4095;
const MAX_WIDTH: usize = 13;
const MAX_DIMENSION: usize = 256 * 1024;
const MIN_EXPONENT: i16 = -149;
const MAX_EXPONENT: i16 = 116;

#[derive(Clone, Copy)]
struct Header {
    base: i32,
    width: usize,
}

impl Header {
    fn coefficients_fit(self) -> bool {
        self.width <= MAX_WIDTH
            && (-MAX_COEFFICIENT..=MAX_COEFFICIENT).contains(&self.base)
            && self.base + ((1_i32 << self.width) - 1) <= MAX_COEFFICIENT
    }
}

/// Interpret the first field of a WAL suffix. The field may be followed by
/// other records or fields. Return only the encoded bytes used by this vector.
pub(crate) fn layout(bytes: &[u8], dim: usize) -> Option<(i16, u8, i16, &[u8])> {
    let (header_bytes, tail) = bytes.split_at_checked(FIELD_HEADER_BYTES)?;
    if header_bytes[0] != 3
        || usize::try_from(u32::from_le_bytes(header_bytes[1..5].try_into().ok()?)).ok()? != dim
    {
        return None;
    }
    let codec_bytes = header_bytes[5..].try_into().ok()?;
    let exponent = i16::from_le_bytes(header_bytes[5..7].try_into().ok()?);
    let width = header_bytes[7];
    let base = i32::from_le_bytes(header_bytes[8..12].try_into().ok()?);
    let header = Header {
        base,
        width: usize::from(width),
    };
    if !(MIN_EXPONENT..=MAX_EXPONENT).contains(&exponent) || !header.coefficients_fit() {
        return None;
    }
    let len = VectorHeader::parse(codec_bytes).payload_len(dim).ok()?;
    let payload = tail.get(..len)?;
    let remainder = (dim * header.width) % 8;
    if remainder != 0 && payload.last().is_some_and(|last| last >> remainder != 0) {
        return None;
    }
    Some((exponent, width, i16::try_from(base).ok()?, payload))
}

#[derive(Clone, Copy)]
pub(crate) struct Kernel(());

impl Kernel {
    pub(crate) fn detect() -> Option<Self> {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            return Some(Self(()));
        }
        None
    }

    /// Convert the original binary32 bits to exact coefficients. A component
    /// outside the proven grid must use the existing float32 exact path.
    pub(crate) fn query(query: &[f32], exponent: i16) -> Option<Vec<i16>> {
        if query.is_empty()
            || query.len() > MAX_DIMENSION
            || !(MIN_EXPONENT..=MAX_EXPONENT).contains(&exponent)
        {
            return None;
        }
        let mut values = Vec::with_capacity(query.len());
        for &value in query {
            values.push(coefficient(value, exponent)?);
        }
        Some(values)
    }

    pub(crate) fn compatible(query: &[f32], exponent: i16) -> bool {
        !query.is_empty()
            && query.len() <= MAX_DIMENSION
            && (MIN_EXPONENT..=MAX_EXPONENT).contains(&exponent)
            && query
                .iter()
                .all(|&value| coefficient(value, exponent).is_some())
    }

    /// The caller obtains width/base and payload from layout and the query
    /// coefficients from query at the same exponent.
    #[cfg_attr(target_arch = "x86_64", allow(unsafe_code))]
    pub(crate) fn distance(self, packed: &[u8], width: u8, base: i16, query: &[i16]) -> u64 {
        let header = Header {
            base: i32::from(base),
            width: usize::from(width),
        };
        assert!(header.coefficients_fit());
        assert!(query.len() <= MAX_DIMENSION);
        assert_eq!(packed.len(), (query.len() * header.width).div_ceil(8));
        debug_assert!(
            query
                .iter()
                .all(|&value| i32::from(value).abs() <= MAX_COEFFICIENT)
        );
        #[cfg(target_arch = "x86_64")]
        {
            // SAFETY: construction requires runtime AVX2 detection. The SIMD
            // implementation only loads within validated slices or stack arrays.
            unsafe { simd::packed(packed, header, query) }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            query
                .iter()
                .enumerate()
                .map(|(i, &q)| squared(scalar_at(packed, i, header), q))
                .sum()
        }
    }
}

fn coefficient(value: f32, exponent: i16) -> Option<i16> {
    let bits = value.to_bits();
    let magnitude = bits & 0x7fff_ffff;
    if bits == 0x8000_0000 || magnitude >= 0x7f80_0000 {
        return None;
    }
    if magnitude == 0 {
        return Some(0);
    }
    let biased = (magnitude >> 23) as i32;
    let (significand, scale) = if biased == 0 {
        (magnitude, -149)
    } else {
        ((magnitude & 0x7f_ffff) | 0x80_0000, biased - 150)
    };
    let zeros = significand.trailing_zeros();
    let odd = significand >> zeros;
    let shift = u32::try_from(scale + zeros as i32 - i32::from(exponent)).ok()?;
    if shift >= u32::BITS || odd > (MAX_COEFFICIENT as u32) >> shift {
        return None;
    }
    let exact = (odd << shift) as i16;
    Some(if bits >> 31 == 0 { exact } else { -exact })
}

#[inline]
fn scalar_at(payload: &[u8], index: usize, header: Header) -> i16 {
    if header.width == 0 {
        return header.base as i16;
    }
    let start = index * header.width;
    let shift = start % 8;
    let bytes = (shift + header.width).div_ceil(8);
    let mut value = 0_u32;
    for (i, &byte) in payload[start / 8..start / 8 + bytes].iter().enumerate() {
        value |= u32::from(byte) << (8 * i);
    }
    let delta = (value >> shift) & ((1_u32 << header.width) - 1);
    (header.base + delta as i32) as i16
}

#[inline]
fn squared(a: i16, b: i16) -> u64 {
    let delta = u64::from(a.abs_diff(b));
    delta * delta
}

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod simd {
    use std::arch::x86_64::*;

    use super::{Header, MAX_WIDTH, scalar_at, squared};

    #[derive(Clone, Copy)]
    struct Unpack {
        shuffle: [u8; 32],
        shifts: [u32; 8],
    }

    const fn plans() -> [Unpack; MAX_WIDTH + 1] {
        let mut plans = [Unpack {
            shuffle: [0x80; 32],
            shifts: [0; 8],
        }; MAX_WIDTH + 1];
        let mut width = 0;
        while width <= MAX_WIDTH {
            let mut lane = 0;
            while lane < 8 {
                let bit = lane * width;
                let byte = bit / 8;
                let shift = bit % 8;
                let count = (shift + width).div_ceil(8);
                plans[width].shifts[lane] = shift as u32;
                let mut i = 0;
                while i < count {
                    plans[width].shuffle[lane * 4 + i] = (byte + i) as u8;
                    i += 1;
                }
                lane += 1;
            }
            width += 1;
        }
        plans
    }
    static PLANS: [Unpack; MAX_WIDTH + 1] = plans();

    #[inline]
    #[target_feature(enable = "avx2")]
    fn load_group(data: &[u8], start: usize, width: usize) -> __m128i {
        let data = &data[start..];
        if let Some(window) = data.get(..16) {
            // SAFETY: window is at least 16 bytes and unaligned loads are supported.
            unsafe { _mm_loadu_si128(window.as_ptr().cast()) }
        } else {
            let mut tail = [0_u8; 16];
            tail[..width].copy_from_slice(&data[..width]);
            // SAFETY: tail has exactly 16 initialized bytes.
            unsafe { _mm_loadu_si128(tail.as_ptr().cast()) }
        }
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn unpack8(
        data: __m128i,
        shuffle: __m256i,
        shift: __m256i,
        mask: __m256i,
        base: __m256i,
    ) -> __m256i {
        let repeated = _mm256_broadcastsi128_si256(data);
        let shuffled = _mm256_shuffle_epi8(repeated, shuffle);
        let deltas = _mm256_and_si256(_mm256_srlv_epi32(shuffled, shift), mask);
        _mm256_add_epi32(deltas, base)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    fn widen(sum: __m256i) -> u64 {
        let mut lanes = [0_i32; 8];
        // SAFETY: lanes has 32 writable bytes.
        unsafe {
            _mm256_storeu_si256(lanes.as_mut_ptr().cast(), sum);
        }
        lanes.into_iter().map(|lane| lane as u64).sum()
    }

    #[target_feature(enable = "avx2")]
    pub(super) fn packed(payload: &[u8], header: Header, query: &[i16]) -> u64 {
        let width = header.width;
        let plan = &PLANS[width];
        // SAFETY: both arrays have 32 initialized bytes.
        let shuffle = unsafe { _mm256_loadu_si256(plan.shuffle.as_ptr().cast()) };
        let shifts = unsafe { _mm256_loadu_si256(plan.shifts.as_ptr().cast()) };
        let mask = _mm256_set1_epi32((1_i32 << width) - 1);
        let base = _mm256_set1_epi32(header.base);
        let mut distance = 0;
        let (groups, remainder) = query.as_chunks::<16>();
        let mut groups_done = 0;
        for chunk in groups.chunks(16) {
            let mut acc = _mm256_setzero_si256();
            for q in chunk {
                let start = groups_done * 2 * width;
                let a = unpack8(
                    load_group(payload, start, width),
                    shuffle,
                    shifts,
                    mask,
                    base,
                );
                let b = unpack8(
                    load_group(payload, start + width, width),
                    shuffle,
                    shifts,
                    mask,
                    base,
                );
                let row = _mm256_permute4x64_epi64::<0xd8>(_mm256_packs_epi32(a, b));
                // SAFETY: q has 16 initialized i16 values.
                let q = unsafe { _mm256_loadu_si256(q.as_ptr().cast()) };
                let diff = _mm256_sub_epi16(row, q);
                acc = _mm256_add_epi32(acc, _mm256_madd_epi16(diff, diff));
                groups_done += 1;
            }
            // 32 * 8190^2 = 2,146,435,200 < i32::MAX per lane.
            distance += widen(acc);
        }
        distance
            + remainder
                .iter()
                .enumerate()
                .map(|(i, &q)| squared(scalar_at(payload, groups_done * 16 + i, header), q))
                .sum::<u64>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(codes: &[i16], header: Header) -> Vec<u8> {
        let mut bytes = vec![0; (header.width * codes.len()).div_ceil(8)];
        for (index, &code) in codes.iter().enumerate() {
            let delta = u32::try_from(i32::from(code) - header.base).unwrap();
            for bit in 0..header.width {
                let at = index * header.width + bit;
                bytes[at / 8] |= (((delta >> bit) & 1) as u8) << (at % 8);
            }
        }
        bytes
    }

    #[test]
    fn distance_matches_scalar_across_widths_tails_and_widening() {
        let Some(kernel) = Kernel::detect() else {
            return;
        };
        for width in 0..MAX_WIDTH {
            for n in [1, 7, 8, 15, 16, 17, 31, 127, 128, 255, 256, 257, 511, 513] {
                let header = Header { base: -4095, width };
                let mask = (1_usize << width) - 1;
                let row: Vec<_> = (0..n)
                    .map(|i| (header.base + (i * 113 % (mask + 1)) as i32) as i16)
                    .collect();
                let query: Vec<_> = (0..n)
                    .map(|i| if i % 2 == 0 { 4095 } else { -4095 })
                    .collect();
                let payload = pack(&row, header);
                let truth: u64 = row.iter().zip(&query).map(|(&a, &b)| squared(a, b)).sum();
                assert_eq!(
                    kernel.distance(&payload, width as u8, -4095, &query),
                    truth,
                    "width {width}, n {n}"
                );
            }
        }
        assert_eq!(
            kernel.distance(&[], 0, -4095, &[4095; 513]),
            513 * 8190_u64 * 8190
        );
    }

    #[test]
    fn layout_limits_its_borrow_and_rejects_unproven_encodings() {
        let mut field = vec![3, 17, 0, 0, 0];
        field.extend_from_slice(&(-10_i16).to_le_bytes());
        field.push(1);
        field.extend_from_slice(&(-2_i32).to_le_bytes());
        field.extend_from_slice(&[0b1010_1010, 0b1010_1010, 1, 0xff]);
        assert_eq!(layout(&field, 17), Some((-10, 1, -2, &field[12..15])));
        assert!(layout(&field[..14], 17).is_none());
        assert!(layout(&field, 18).is_none());
        field[14] = 2;
        assert!(layout(&field, 17).is_none());
        field[14] = 1;
        field[8..12].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(layout(&field, 17).is_none());
        field[8..12].copy_from_slice(&(-4095_i32).to_le_bytes());
        field[7] = 13;
        assert!(layout(&field, 17).is_none());
    }

    #[test]
    fn query_conversion_preserves_exact_binary32_values_on_the_grid() {
        let tiniest = f32::from_bits(1);
        assert_eq!(
            Kernel::query(&[0.0, tiniest, -tiniest, f32::from_bits(4095)], -149),
            Some(vec![0, 1, -1, 4095])
        );
        assert!(Kernel::query(&[f32::from_bits(4096)], -149).is_none());
        assert!(Kernel::query(&[tiniest], -148).is_none());
        assert!(Kernel::query(&[-0.0], -149).is_none());
        assert!(Kernel::query(&[f32::INFINITY], -149).is_none());
        assert!(Kernel::query(&[f32::NAN], -149).is_none());
        assert_eq!(Kernel::query(&[-0.5, 0.75, 1.0], -2), Some(vec![-2, 3, 4]));
        assert!(Kernel::query(&[-0.5, 0.75, 1.0], -1).is_none());
        assert_eq!(
            Kernel::query(&[4095.0 * 2.0_f32.powi(116)], 116),
            Some(vec![4095])
        );
        assert!(Kernel::query(&[0.0], 117).is_none());
    }
}
