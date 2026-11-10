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
    #[cfg(target_arch = "x86_64")]
    avx: Option<avx::Kernel>,
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
            #[cfg(target_arch = "x86_64")]
            avx: avx::Kernel::detect(),
        })
    }

    pub fn estimate(&self, row: &[f32], q: &[f32]) -> f64 {
        debug_assert_eq!(row.len(), q.len());
        #[cfg(target_arch = "x86_64")]
        if let Some(kernel) = &self.avx {
            return kernel.estimate(row, q);
        }
        estimate_scalar(row, q)
    }

    pub fn lower(&self, estimate: f64) -> f64 {
        (estimate * self.lower_factor).next_down().max(0.0)
    }

    pub fn upper(&self, estimate: f64) -> f64 {
        (estimate * self.upper_factor).next_up()
    }
}

fn estimate_scalar(row: &[f32], q: &[f32]) -> f64 {
    let (row_chunks, row_tail) = row.as_chunks::<8>();
    let (q_chunks, q_tail) = q.as_chunks::<8>();
    let mut sums = [0.0; 8];
    for (a, b) in row_chunks.iter().zip(q_chunks) {
        for lane in 0..8 {
            let delta = f64::from(a[lane]) - f64::from(b[lane]);
            sums[lane] += delta * delta;
        }
    }
    reduce(sums, row_tail, q_tail)
}

fn reduce(mut sums: [f64; 8], row_tail: &[f32], q_tail: &[f32]) -> f64 {
    for (&a, &b) in row_tail.iter().zip(q_tail) {
        let delta = f64::from(a) - f64::from(b);
        sums[0] += delta * delta;
    }
    sums.into_iter().sum()
}

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod avx {
    use std::arch::x86_64::{
        _mm_setr_ps, _mm256_add_pd, _mm256_cvtps_pd, _mm256_mul_pd, _mm256_setzero_pd,
        _mm256_storeu_pd, _mm256_sub_pd,
    };

    pub(super) struct Kernel(());

    impl Kernel {
        pub(super) fn detect() -> Option<Self> {
            std::is_x86_feature_detected!("avx").then_some(Self(()))
        }

        pub(super) fn estimate(&self, row: &[f32], q: &[f32]) -> f64 {
            // SAFETY: the only constructor checks OS support and AVX availability.
            unsafe { estimate(row, q) }
        }
    }

    #[target_feature(enable = "avx")]
    fn estimate(row: &[f32], q: &[f32]) -> f64 {
        let (rows, row_tail) = row.as_chunks::<8>();
        let (queries, q_tail) = q.as_chunks::<8>();
        let mut low = _mm256_setzero_pd();
        let mut high = _mm256_setzero_pd();
        for (&[a0, a1, a2, a3, a4, a5, a6, a7], &[b0, b1, b2, b3, b4, b5, b6, b7]) in
            rows.iter().zip(queries)
        {
            let a_lo = _mm256_cvtps_pd(_mm_setr_ps(a0, a1, a2, a3));
            let b_lo = _mm256_cvtps_pd(_mm_setr_ps(b0, b1, b2, b3));
            let a_hi = _mm256_cvtps_pd(_mm_setr_ps(a4, a5, a6, a7));
            let b_hi = _mm256_cvtps_pd(_mm_setr_ps(b4, b5, b6, b7));
            let d_lo = _mm256_sub_pd(a_lo, b_lo);
            let d_hi = _mm256_sub_pd(a_hi, b_hi);
            low = _mm256_add_pd(low, _mm256_mul_pd(d_lo, d_lo));
            high = _mm256_add_pd(high, _mm256_mul_pd(d_hi, d_hi));
        }
        let mut sums = [0.0; 8];
        let (first, last) = sums.split_at_mut(4);
        // SAFETY: each slice owns four writable f64s. The stores are unaligned.
        unsafe {
            _mm256_storeu_pd(first.as_mut_ptr(), low);
            _mm256_storeu_pd(last.as_mut_ptr(), high);
        }
        // Same lane grouping, tail and reduction order as the scalar kernel.
        super::reduce(sums, row_tail, q_tail)
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

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx_estimate_matches_scalar_bits_for_full_and_partial_blocks() {
        let Some(kernel) = avx::Kernel::detect() else {
            return;
        };
        let mut random = 0xe567_32be_u32;
        let mut sample = || {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            let mut bits = random;
            if bits & 0x7f80_0000 == 0x7f80_0000 {
                bits ^= 0x0080_0000;
            }
            f32::from_bits(bits)
        };
        for dim in 1..=257 {
            for _ in 0..8 {
                let a: Vec<_> = (0..dim).map(|_| sample()).collect();
                let b: Vec<_> = (0..dim).map(|_| sample()).collect();
                assert_eq!(
                    kernel.estimate(&a, &b).to_bits(),
                    estimate_scalar(&a, &b).to_bits(),
                    "dimension {dim}"
                );
            }
        }
    }
}
