//! Finite, bit-exact conversion for hot vectors.
pub(crate) fn pack(value: f32) -> Option<u16> {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let magnitude = bits & 0x7fff_ffff;
    if magnitude == 0 {
        return Some(sign);
    }
    let exponent = (magnitude >> 23) as i32 - 127;
    let fraction = bits & 0x7f_ffff;
    match exponent {
        -14..=15 if fraction & 0x1fff == 0 => {
            Some(sign | (((exponent + 15) as u16) << 10) | ((fraction >> 13) as u16))
        }
        -24..=-15 => {
            let shift = (-exponent - 1) as u32;
            let significand = fraction | 0x80_0000;
            (significand & ((1 << shift) - 1) == 0)
                .then_some(sign | ((significand >> shift) as u16))
        }
        _ => None,
    }
}

pub(crate) fn expand(half: u16) -> f32 {
    let sign = (u32::from(half) & 0x8000) << 16;
    let exponent = (u32::from(half) >> 10) & 31;
    let fraction = u32::from(half) & 1023;
    let magnitude = if exponent != 0 {
        ((exponent + 112) << 23) | (fraction << 13)
    } else if fraction != 0 {
        let leading = 31 - fraction.leading_zeros();
        ((103 + leading) << 23) | ((fraction << (23 - leading)) & 0x7f_ffff)
    } else {
        0
    };
    f32::from_bits(sign | magnitude)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_finite_half_and_neighboring_float_round_trips_exactly() {
        for bits in 0..=u16::MAX {
            if bits & 0x7c00 == 0x7c00 {
                continue;
            }
            let value = expand(bits);
            assert!(value.is_finite());
            assert_eq!(pack(value), Some(bits));
            if value != 0.0 {
                assert!(pack(f32::from_bits(value.to_bits() + 1)).is_none());
            }
            if bits & 0x7c00 == 0 {
                let sign = if bits >> 15 == 0 { 1.0 } else { -1.0 };
                assert_eq!(
                    value.to_bits(),
                    (sign * f32::from(bits & 1023) * 2.0_f32.powi(-24)).to_bits()
                );
            }
        }
        for value in [
            f32::from_bits(1),
            65536.0,
            f32::MAX,
            f32::INFINITY,
            f32::NAN,
        ] {
            assert!(pack(value).is_none());
        }
    }
}
