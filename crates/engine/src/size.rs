//! The scale of a size, in decimal units: a scale, the words belong to the
//! binaries, which name the units in their language.

/// How many of a unit make the next one.
const STEP: u64 = 1000;

/// How many tenths make a unit.
const TENTHS: u64 = 10;

/// A decimal unit of size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeUnit {
    Byte,
    Kilobyte,
    Megabyte,
    Gigabyte,
    Terabyte,
}

impl SizeUnit {
    /// Each unit, the smallest first.
    const ALL: [Self; 5] = [
        Self::Byte,
        Self::Kilobyte,
        Self::Megabyte,
        Self::Gigabyte,
        Self::Terabyte,
    ];
}

/// A number of bytes in the largest unit it reaches (up to the terabyte),
/// with its tenths truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize {
    whole: u64,
    /// Below [`TENTHS`]; always 0 in bytes.
    tenths: u64,
    unit: SizeUnit,
}

impl ByteSize {
    #[must_use]
    pub fn of(bytes: u64) -> Self {
        let mut unit = 0;
        let mut whole = bytes;
        let mut tenths = 0;
        while whole >= STEP && unit + 1 < SizeUnit::ALL.len() {
            tenths = (whole % STEP) / (STEP / TENTHS);
            whole /= STEP;
            unit += 1;
        }
        Self {
            whole,
            tenths,
            unit: SizeUnit::ALL[unit],
        }
    }

    /// The whole units.
    #[must_use]
    pub const fn whole(self) -> u64 {
        self.whole
    }

    /// The tenth of a unit, truncated; none for bytes, never split.
    #[must_use]
    pub const fn tenths(self) -> Option<u64> {
        match self.unit {
            SizeUnit::Byte => None,
            _ => Some(self.tenths),
        }
    }

    #[must_use]
    pub const fn unit(self) -> SizeUnit {
        self.unit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(bytes: u64) -> (u64, Option<u64>, SizeUnit) {
        let size = ByteSize::of(bytes);
        (size.whole(), size.tenths(), size.unit())
    }

    #[test]
    fn scales_to_the_largest_unit_reached_truncating_the_tenth() {
        assert_eq!(parts(0), (0, None, SizeUnit::Byte));
        assert_eq!(parts(999), (999, None, SizeUnit::Byte));
        assert_eq!(parts(1000), (1, Some(0), SizeUnit::Kilobyte));
        assert_eq!(parts(1_999), (1, Some(9), SizeUnit::Kilobyte));
        assert_eq!(parts(999_999), (999, Some(9), SizeUnit::Kilobyte));
        assert_eq!(parts(1_999_999), (1, Some(9), SizeUnit::Megabyte));
        assert_eq!(parts(2_340_000_000), (2, Some(3), SizeUnit::Gigabyte));
        assert_eq!(
            parts(u64::MAX),
            (18_446_744, Some(0), SizeUnit::Terabyte),
            "the terabyte is the largest unit"
        );
    }
}
