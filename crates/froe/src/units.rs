//! Rendering byte counts for people to read.
//!
//! A repository is measured in tens of gigabytes and its archives in
//! hundreds of megabytes, so the figures froe reports are routinely ten and
//! eleven digits long. Printed raw they are unreadable at a glance and
//! impossible to compare against each other, which is the whole reason an
//! operator reads them.
//!
//! Binary (IEC) units, not decimal ones: the format is binary throughout —
//! a segment caps at 262144 bytes and Oak rotates an archive at 256 MiB —
//! and an operator comparing froe's figure against `du` needs the ambiguity
//! of "GB" gone.

/// A byte count scaled to binary units and truncated toward zero, so the
/// scaled figure never overstates the size: `0 bytes`, `512 bytes`,
/// `47.7 MiB`, `54.7 GiB`.
///
/// Counts below one kibibyte keep their exact value and the plain noun,
/// because at that size the exact number is both short and the more useful
/// one. Everything larger carries one decimal place.
///
/// The arithmetic is integer arithmetic in `u128`, never floating point, so
/// the printed digits are a function of the input alone and the largest
/// counts cannot lose precision on their way to the terminal.
#[must_use]
pub fn format_byte_size(bytes: u64) -> String {
    /// Suffix and divisor for each binary unit, largest first.
    const UNITS: [(&str, u64); 6] = [
        ("EiB", 1 << 60),
        ("PiB", 1 << 50),
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];

    for (suffix, divisor) in UNITS {
        if bytes >= divisor {
            // Truncating, so a figure never claims more than is there.
            let tenths = u128::from(bytes) * 10 / u128::from(divisor);
            return format!("{}.{} {suffix}", tenths / 10, tenths % 10);
        }
    }
    if bytes == 1 {
        return "1 byte".to_owned();
    }
    format!("{bytes} bytes")
}

#[cfg(test)]
mod tests {
    use super::format_byte_size;

    /// Expectations computed by hand rather than by running the code under
    /// test, including every figure from the field report that motivated the
    /// scaled rendering.
    #[test]
    fn scaled_sizes_match_a_hand_computed_table() {
        let table = [
            (0u64, "0 bytes"),
            (1, "1 byte"),
            (2, "2 bytes"),
            (512, "512 bytes"),
            (1023, "1023 bytes"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (19456, "19.0 KiB"),
            (1_048_576, "1.0 MiB"),
            (9_667_072, "9.2 MiB"),
            (50_022_400, "47.7 MiB"),
            (55_150_080, "52.5 MiB"),
            (642_241_536, "612.4 MiB"),
            (1_073_741_824, "1.0 GiB"),
            (24_131_206_144, "22.4 GiB"),
            (34_667_664_384, "32.2 GiB"),
            (58_798_870_528, "54.7 GiB"),
            (1 << 40, "1.0 TiB"),
            (1 << 50, "1.0 PiB"),
            (1 << 60, "1.0 EiB"),
            (u64::MAX, "15.9 EiB"),
        ];
        for (bytes, expected) in table {
            assert_eq!(format_byte_size(bytes), expected, "for {bytes} bytes");
        }
    }

    /// An inverse check computed from the printed text: re-read the mantissa
    /// and the suffix, and require the size to sit inside the tenth of a unit
    /// the output claims. This never re-runs the formatter's own arithmetic.
    #[test]
    fn a_scaled_size_never_overstates_the_bytes_it_describes() {
        let divisor_for = |suffix: &str| -> u128 {
            match suffix {
                "KiB" => 1 << 10,
                "MiB" => 1 << 20,
                "GiB" => 1 << 30,
                "TiB" => 1 << 40,
                "PiB" => 1 << 50,
                "EiB" => 1 << 60,
                other => panic!("unexpected unit suffix {other:?}"),
            }
        };

        // A deterministic spread: powers of two, their neighbours, and a
        // seeded pseudo-random sweep across the whole domain.
        let mut sizes: Vec<u64> = Vec::new();
        for exponent in 0..64 {
            let value = 1u64 << exponent;
            sizes.extend([value.saturating_sub(1), value, value.saturating_add(1)]);
        }
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            sizes.push(state);
        }

        for bytes in sizes {
            let rendered = format_byte_size(bytes);
            let (value, suffix) = rendered
                .split_once(' ')
                .expect("a rendered size is a value and a unit");
            if suffix == "bytes" || suffix == "byte" {
                assert!(bytes < 1024, "{bytes} was rendered unscaled as {rendered}");
                assert_eq!(value.parse::<u64>(), Ok(bytes));
                continue;
            }
            let divisor = divisor_for(suffix);
            let (whole, fraction) = value
                .split_once('.')
                .expect("a scaled size carries one decimal place");
            assert_eq!(fraction.len(), 1, "exactly one decimal place: {rendered}");
            let tenths = u128::from(whole.parse::<u64>().expect("whole part")) * 10
                + u128::from(fraction.parse::<u64>().expect("fractional part"));
            let claimed = tenths * divisor / 10;
            let next = (tenths + 1) * divisor / 10;
            assert!(
                claimed <= u128::from(bytes) && u128::from(bytes) < next,
                "{bytes} rendered as {rendered}, which describes [{claimed}, {next})"
            );
            assert!(
                tenths >= 10,
                "{bytes} rendered as {rendered}, below its own unit"
            );
        }
    }
}
