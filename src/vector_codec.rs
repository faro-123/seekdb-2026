//! Lossless per-vector binary scale and frame-of-reference encoding.
use crate::{Error, Result};

pub(crate) const HEADER_BYTES: usize = 7;
const MAX_PACKED_DIM: usize = 256 * 1024;
const FRACTION: u32 = (1 << 23) - 1;

fn invalid() -> Error {
    Error::new("storage_error", "invalid packed vector")
}

fn power_of_two(exponent: i32) -> f64 {
    // Callers bound exponents to the finite f32 range (including subnormals).
    f64::from_bits(((1023 + exponent) as u64) << 52)
}

#[derive(Clone, Copy)]
pub(crate) struct Header {
    exponent: i16,
    width: u8,
    base: i32,
}

impl Header {
    pub fn parse(bytes: &[u8; HEADER_BYTES]) -> Self {
        Self {
            exponent: i16::from_le_bytes([bytes[0], bytes[1]]),
            width: bytes[2],
            base: i32::from_le_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]),
        }
    }

    pub fn payload_len(self, count: usize) -> Result<usize> {
        if count == 0
            || count > MAX_PACKED_DIM
            || !(-149..=127).contains(&self.exponent)
            || self.width >= 32
        {
            return Err(invalid());
        }
        let bytes = (count * usize::from(self.width)).div_ceil(8);
        if HEADER_BYTES + bytes >= count * 4 {
            return Err(invalid());
        }
        Ok(bytes)
    }

    pub fn decode(self, count: usize, payload: &[u8], out: &mut Vec<f32>) -> Result<()> {
        if payload.len() != self.payload_len(count)? {
            return Err(invalid());
        }
        let scale = power_of_two(i32::from(self.exponent));
        let width = self.width;
        let mask = (1_u64 << width) - 1;
        let mut bucket = 0_u64;
        let mut bits = 0;
        let mut cursor = 0;
        out.reserve(count);
        for _ in 0..count {
            while bits < width {
                bucket |= u64::from(payload[cursor]) << bits;
                cursor += 1;
                bits += 8;
            }
            let delta = (bucket & mask) as i64;
            let coefficient = i32::try_from(i64::from(self.base) + delta).map_err(|_| invalid())?;
            let exact = f64::from(coefficient) * scale;
            let value = exact as f32;
            if f64::from(value) != exact {
                return Err(invalid());
            }
            out.push(value);
            bucket >>= width;
            bits -= width;
        }
        if bucket != 0 {
            return Err(invalid());
        }
        Ok(())
    }
}

// Writes nothing on fallback. The old raw field remains byte-for-byte identical.
pub(crate) fn try_pack(vector: &[f32], out: &mut Vec<u8>) -> bool {
    if vector.len() * 4 <= HEADER_BYTES || vector.len() > MAX_PACKED_DIM {
        return false;
    }

    let mut exponent = 127;
    let mut low = vector[0];
    let mut high = low;
    for &value in vector {
        let bits = value.to_bits();
        let magnitude = bits & 0x7fff_ffff;
        if bits == 0x8000_0000 || magnitude >= 0x7f80_0000 {
            return false;
        }
        low = low.min(value);
        high = high.max(value);
        if magnitude == 0 {
            continue;
        }
        let biased = (magnitude >> 23) as i32;
        let (mantissa, e) = if biased == 0 {
            (magnitude, -149)
        } else {
            ((magnitude & FRACTION) | (1 << 23), biased - 150)
        };
        exponent = exponent.min(e + mantissa.trailing_zeros() as i32);
    }

    // Multiplying an f32 by a binary power in f64 is exact here. The exponent
    // above makes each result integral; the extrema bound every coefficient.
    let inverse = power_of_two(-exponent);
    let low = f64::from(low) * inverse;
    let high = f64::from(high) * inverse;
    if low < f64::from(i32::MIN) || high > f64::from(i32::MAX) {
        return false;
    }
    let base = low as i32;
    let range = (high as i32).abs_diff(base);
    let width = (32 - range.leading_zeros()) as u8;
    let raw_bytes = vector.len() * 4;
    let packed_bytes = HEADER_BYTES + (vector.len() * usize::from(width)).div_ceil(8);
    // Pay codec costs only when the payload shrinks by at least one eighth.
    if packed_bytes > raw_bytes - raw_bytes.div_ceil(8) {
        return false;
    }

    out.reserve(packed_bytes);
    out.extend_from_slice(&(exponent as i16).to_le_bytes());
    out.push(width);
    out.extend_from_slice(&base.to_le_bytes());
    if width == 0 {
        return true;
    }
    let mut bucket = 0_u64;
    let mut bits = 0;
    for &value in vector {
        let coefficient = (f64::from(value) * inverse) as i32;
        let delta = coefficient.abs_diff(base);
        bucket |= u64::from(delta) << bits;
        bits += width;
        while bits >= 8 {
            out.push(bucket as u8);
            bucket >>= 8;
            bits -= 8;
        }
    }
    if bits != 0 {
        out.push(bucket as u8);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(values: &[f32], packed: Option<bool>) {
        let mut encoded = Vec::new();
        let actual = try_pack(values, &mut encoded);
        if let Some(expected) = packed {
            assert_eq!(actual, expected);
        }
        if actual {
            let (header, body) = encoded.split_first_chunk().unwrap();
            let mut decoded = Vec::new();
            Header::parse(header)
                .decode(values.len(), body, &mut decoded)
                .unwrap();
            assert!(
                values
                    .iter()
                    .map(|v| v.to_bits())
                    .eq(decoded.iter().map(|v| v.to_bits()))
            );
        } else {
            assert!(encoded.is_empty());
        }
    }

    #[test]
    fn preserves_bits_for_grid_constants_subnormals_and_fallbacks() {
        for count in 1..=4097 {
            check(&vec![f32::from_bits(1); count], Some(count > 1));
        }
        check(&vec![f32::MAX; 128], Some(true));
        check(&[3.0, 0.0, 3.0, 0.0], Some(true));
        check(&[-3.0, 0.0, -3.0, 0.0], Some(true));
        check(&vec![-0.0; 128], Some(false));
        check(&vec![0.0; MAX_PACKED_DIM + 1], Some(false));
        check(
            &[0.0, -0.0, f32::MAX, f32::MIN, f32::from_bits(1)],
            Some(false),
        );
        let mut state = 19_u32;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        for exponent in [-149, -126, -22, -1, 0, 30, 110] {
            for count in [2, 5, 17, 63, 128, 137, 2049] {
                let values: Vec<_> = (0..count)
                    .map(|_| ((f64::from(next() % 4096) - 2048.0) * power_of_two(exponent)) as f32)
                    .collect();
                check(&values, (count >= 63).then_some(true));
            }
        }
        for count in [17, 65, 128, 4096] {
            let values: Vec<_> = (0..count)
                .map(|_| {
                    let bits = next();
                    f32::from_bits(if bits & 0x7f80_0000 == 0x7f80_0000 {
                        bits ^ 0x0080_0000
                    } else {
                        bits
                    })
                })
                .collect();
            check(&values, Some(false));
        }
    }

    #[test]
    fn rejects_bad_header_payload_padding_and_unrepresentable_values() {
        let header = Header {
            exponent: 0,
            width: 1,
            base: 0,
        };
        let mut out = Vec::new();
        assert!(header.decode(17, &[0, 0], &mut out).is_err());
        assert!(header.decode(17, &[0, 0, 2], &mut out).is_err());
        assert!(
            Header {
                width: 32,
                ..header
            }
            .payload_len(128)
            .is_err()
        );
        assert!(
            Header {
                exponent: -150,
                ..header
            }
            .payload_len(128)
            .is_err()
        );
        assert!(header.payload_len(MAX_PACKED_DIM + 1).is_err());
        assert!(
            Header {
                base: i32::MAX,
                ..header
            }
            .decode(17, &[1, 0, 0], &mut out)
            .is_err()
        );
        assert!(
            Header {
                base: 16_777_216,
                ..header
            }
            .decode(17, &[1, 0, 0], &mut out)
            .is_err()
        );
    }
}
