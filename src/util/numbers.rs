use std::fmt;

/// Round `n` to two decimal places.
pub fn round_2_decimals(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

pub fn with_comma_int<T: Int>(n: T) -> FormatInt {
    FormatInt(n.into_i128())
}

pub struct FormatInt(i128);

impl fmt::Display for FormatInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0");
        }

        let mut n = if self.0 < 0 {
            f.write_str("-")?;

            self.0.unsigned_abs()
        } else {
            self.0 as u128
        };

        let mut chunks = [0_u16; 14];
        let mut count = 0_usize;

        while n > 0 {
            chunks[count] = (n % 1000) as u16;
            n /= 1000;
            count += 1;
        }

        write!(f, "{}", chunks[count - 1])?;

        for &chunk in chunks[..count - 1].iter().rev() {
            write!(f, ",{chunk:0>3}")?;
        }

        Ok(())
    }
}

pub trait Int {
    fn into_i128(self) -> i128;
}

macro_rules! into_int {
    ($ty:ty) => {
        impl Int for $ty {
            fn into_i128(self) -> i128 {
                self as i128
            }
        }
    };
}

into_int!(u8);
into_int!(u16);
into_int!(u32);
into_int!(u64);
into_int!(usize);

into_int!(i8);
into_int!(i16);
into_int!(i32);
into_int!(i64);
into_int!(isize);

pub fn last_multiple(per_page: usize, total: usize) -> usize {
    if per_page == 0 || total == 0 {
        return 0;
    }

    if per_page <= total && total.is_multiple_of(per_page) {
        total - per_page
    } else {
        total - total % per_page
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_2_decimals() {
        let v1 = 3.1615;
        let v2 = 3.16;

        if (round_2_decimals(v1) - v2).abs() > f64::EPSILON {
            panic!(
                "[test_round_2_decimals] round_2_decimals({v1})={} != {v2}",
                round_2_decimals(v1)
            );
        }
    }

    #[test]
    fn test_with_comma_int() {
        assert_eq!(with_comma_int(0_u64).to_string(), "0".to_owned());
        assert_eq!(with_comma_int(-1000_i64).to_string(), "-1,000".to_owned());
        assert_eq!(
            with_comma_int(i64::MIN).to_string(),
            "-9,223,372,036,854,775,808".to_owned()
        );
        assert_eq!(
            with_comma_int(9_000_000_000_000_000_999_i64).to_string(),
            "9,000,000,000,000,000,999".to_owned()
        );
        assert_eq!(
            with_comma_int(u64::MAX).to_string(),
            "18,446,744,073,709,551,615".to_owned()
        );
        assert_eq!(
            with_comma_int(31_415_926_u32).to_string(),
            "31,415,926".to_owned()
        );
        assert_eq!(last_multiple(0, 10), 0);
        assert_eq!(last_multiple(10, 0), 0);
        assert_eq!(last_multiple(10, 20), 10);
    }
}
