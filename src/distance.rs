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
            if i == j {
                let diff = if nx == ny { x.abs_diff(y) } else { x + y };
                sum.apply(diff * diff, 2 * i, false);
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
