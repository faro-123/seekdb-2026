//! Exact squared distance keys for finite float32 vectors.
use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

// Each float32 is a signed integer times 2^-149. A squared difference
// needs at most 556 bits in units of 2^-298, and a row has fewer than 2^32
// dimensions in the storage format. Ten limbs leave room for the sum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SquaredL2([u64; 10]);

fn parts(value: f32) -> (u64, usize, bool) {
    let bits = value.to_bits();
    let exponent = ((bits >> 23) & 255) as usize;
    let fraction = u64::from(bits & 0x7f_ffff);
    let significand = if exponent == 0 {
        fraction
    } else {
        fraction | 0x80_0000
    };
    (significand, exponent.saturating_sub(1), bits >> 31 != 0)
}

impl SquaredL2 {
    pub fn between(row: &[f32], query: &[f32]) -> Self {
        let mut sum = Self([0; 10]);
        for (&a, &b) in row.iter().zip(query) {
            let (x, i, nx) = parts(a);
            let (y, j, ny) = parts(b);
            if i.abs_diff(j) <= 7 {
                // With 24-bit significands and at most seven alignment bits,
                // the sum is below 2^32, so its square fits in one u64.
                let common = i.min(j);
                let x = x << (i - common);
                let y = y << (j - common);
                let diff = if nx == ny { x.abs_diff(y) } else { x + y };
                sum.apply(diff * diff, 2 * common, false);
            } else {
                // (a-b)^2 = a^2+b^2-2ab. Add both squares before subtracting.
                sum.apply(x * x, 2 * i, false);
                sum.apply(y * y, 2 * j, false);
                sum.apply(2 * x * y, i + j, nx == ny);
            }
        }
        sum
    }

    fn apply(&mut self, significand: u64, shift: usize, subtract: bool) {
        let limb = shift / 64;
        let bits = shift % 64;
        self.add_word(limb, significand << bits, subtract);
        if bits != 0 {
            self.add_word(limb + 1, significand >> (64 - bits), subtract);
        }
    }

    fn add_word(&mut self, mut limb: usize, mut word: u64, subtract: bool) {
        while word != 0 {
            let (next, carry) = if subtract {
                self.0[limb].overflowing_sub(word)
            } else {
                self.0[limb].overflowing_add(word)
            };
            self.0[limb] = next;
            word = u64::from(carry);
            limb += 1;
        }
    }

    pub fn distance(self) -> f64 {
        const LIMB_BASE: f64 = 18446744073709551616.0; // 2^64
        const SCALE: f64 = f64::from_bits((1023 - 298) << 52);
        let magnitude = self
            .0
            .iter()
            .rev()
            .fold(0.0, |acc, &limb| acc * LIMB_BASE + limb as f64);
        (magnitude * SCALE).sqrt()
    }
}

impl Ord for SquaredL2 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.iter().rev().cmp(other.0.iter().rev())
    }
}

impl PartialOrd for SquaredL2 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) struct DistanceBounds {
    lower_factor: f64,
    upper_factor: f64,
}

impl DistanceBounds {
    pub fn new(dim: usize) -> Option<Self> {
        if !(1..1_000_000).contains(&dim) {
            return None;
        }
        // f32 widens exactly. Every nonzero delta has magnitude at least 2^-149
        // and at most 2^129. Squares and their positive sums are normal f64s.
        // In round-to-nearest, each term sees two subtraction error factors, one
        // multiplication, at most dim lane additions and eight final additions.
        // Thus estimate/exact lies in [1-gamma,1+gamma] with
        // gamma = (dim+11)*u/(1-(dim+11)*u), u = EPSILON/2.
        // This exactly representable slack exceeds gamma/(1-gamma) throughout
        // the supported range, covering inversion of the relative-error bound.
        let slack = 16.0 * (dim as f64 + 4.0) * f64::EPSILON;
        Some(Self {
            lower_factor: 1.0 - slack,
            upper_factor: 1.0 + slack,
        })
    }

    pub fn estimate(&self, row: &[f32], q: &[f32]) -> f64 {
        debug_assert_eq!(row.len(), q.len());
        let (row_chunks, row_tail) = row.as_chunks::<8>();
        let (q_chunks, q_tail) = q.as_chunks::<8>();
        let mut sums = [0.0; 8];
        for (a, b) in row_chunks.iter().zip(q_chunks) {
            for lane in 0..8 {
                let delta = f64::from(a[lane]) - f64::from(b[lane]);
                sums[lane] += delta * delta;
            }
        }
        for (&a, &b) in row_tail.iter().zip(q_tail) {
            let delta = f64::from(a) - f64::from(b);
            sums[0] += delta * delta;
        }
        sums.into_iter().sum()
    }

    pub fn lower(&self, estimate: f64) -> f64 {
        (estimate * self.lower_factor).next_down().max(0.0)
    }

    pub fn upper(&self, estimate: f64) -> f64 {
        (estimate * self.upper_factor).next_up()
    }
}

#[cfg(test)]
mod screening_tests {
    use super::*;

    #[test]
    fn interval_encloses_nonzero_subnormal_squared_distance() {
        let bounds = DistanceBounds::new(17).expect("17 dimensions must support screening");
        let mut row = [0.0; 17];
        row[16] = f32::from_bits(1);
        let estimate = bounds.estimate(&row, &[0.0; 17]);
        let squared = 2.0_f64.powi(-298);
        assert!(bounds.lower(estimate) <= squared && squared <= bounds.upper(estimate));
        assert!(bounds.lower(estimate) > 0.0);
    }
}
