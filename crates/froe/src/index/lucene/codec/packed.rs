//! The packed-integer writers every Lucene format embeds.
//!
//! `docs/analysis/lucene-4-7-codec.md` §2, from
//! `util/packed/PackedInts.java`, `AbstractBlockPackedWriter.java`,
//! `BlockPackedWriter.java` and `MonotonicBlockPackedWriter.java`.
//!
//! Three layers: a header-less bit packer, a block writer that
//! delta-encodes each block against its own minimum, and a monotonic one
//! for a stream that only increases.
//!
//! **froe writes the `PACKED` format only.** `PACKED_SINGLE_BLOCK` is what
//! Lucene's `fastestFormatAndBits` selects for 1, 2 and 4 bits under a
//! speed-weighted budget; the reader dispatches on the format id the
//! enclosing format recorded, so a `PACKED`-only writer is valid everywhere.
//! A recorded choice, not an oversight — §10.3.

use std::io::Write;

use crate::error::Result;
use crate::index::lucene::codec::data_output::CodecOutput;

/// `PackedInts.VERSION_CURRENT` (`VERSION_BYTE_ALIGNED`).
pub const PACKED_VERSION_CURRENT: i32 = 1;

/// `PackedInts.Format.PACKED.getId()`.
pub const FORMAT_PACKED: i32 = 0;

/// `AbstractBlockPackedWriter.MIN_BLOCK_SIZE`.
pub const MIN_BLOCK_SIZE: usize = 64;

/// `AbstractBlockPackedWriter.MIN_VALUE_EQUALS_0`.
const MIN_VALUE_EQUALS_0: u32 = 1;

/// `AbstractBlockPackedWriter.BPV_SHIFT`.
const BPV_SHIFT: u32 = 1;

/// `PackedInts.bitsRequired`: never zero, because it is `max(1, …)`.
///
/// The block writers have their own zero cases and do not call this for an
/// all-equal block.
#[must_use]
pub fn bits_required(max_value: u64) -> u32 {
    (64 - max_value.leading_zeros()).max(1)
}

/// `PackedInts.maxValue`.
#[must_use]
pub const fn max_value(bits_per_value: u32) -> u64 {
    if bits_per_value >= 64 {
        i64::MAX as u64
    } else {
        !(!0u64 << bits_per_value)
    }
}

/// `PackedInts.Format.PACKED.byteCount` at `VERSION_CURRENT`.
///
/// **Byte**-aligned, not long-aligned: `ceil(count * bits / 8)`. A writer
/// that pads to eight bytes makes every later offset in the enclosing file
/// wrong.
#[must_use]
pub const fn packed_byte_count(value_count: usize, bits_per_value: u32) -> usize {
    (value_count * bits_per_value as usize).div_ceil(8)
}

/// `AbstractBlockPackedWriter.zigZagEncode`.
#[must_use]
pub const fn zig_zag_encode(value: i64) -> i64 {
    (value >> 63) ^ (value << 1)
}

/// Writes `values` contiguously at `bits_per_value`, with no header.
///
/// This is `PackedInts.getWriterNoHeader(…, Format.PACKED, …)` followed by
/// `finish`: the bits run together across byte boundaries, most significant
/// bit of the first value first, and the last byte is zero-padded.
pub fn write_packed<Sink: Write>(
    output: &mut CodecOutput<Sink>,
    values: &[u64],
    bits_per_value: u32,
) -> Result<()> {
    if bits_per_value == 0 || bits_per_value > 64 {
        return Err(crate::error::Error::InvalidFormat {
            details: format!("{bits_per_value} bits per value is outside 1..=64"),
        });
    }
    let mut buffer = vec![0u8; packed_byte_count(values.len(), bits_per_value)];
    let mut bit = 0usize;
    for value in values {
        // Most significant bit first, so a value straddling a byte boundary
        // continues into the next byte's high bits — which is what makes the
        // stream readable without knowing where a value started.
        for offset in (0..bits_per_value).rev() {
            if (value >> offset) & 1 == 1 {
                buffer[bit / 8] |= 0x80 >> (bit % 8);
            }
            bit += 1;
        }
    }
    output.write_bytes(&buffer)
}

/// `BlockPackedWriter`: one self-describing block per call.
///
/// The caller supplies a whole block. Lucene buffers to a block size and
/// flushes; froe's callers already have their values, so the block boundary
/// is theirs to choose and the block size is not state here.
pub fn write_block_packed<Sink: Write>(
    output: &mut CodecOutput<Sink>,
    values: &[i64],
) -> Result<()> {
    if values.is_empty() {
        return Err(crate::error::Error::InvalidFormat {
            details: "a block-packed block holds at least one value".to_owned(),
        });
    }

    let minimum = values.iter().copied().min().unwrap_or(0);
    let maximum = values.iter().copied().max().unwrap_or(0);

    // Signed overflow is not an error here: it means the block spans more
    // than `Long.MAX_VALUE`, and Lucene gives up on delta-encoding.
    let delta = maximum.wrapping_sub(minimum);
    let bits = match delta {
        negative if negative < 0 => 64,
        0 => 0,
        positive => bits_required(positive as u64),
    };

    let minimum = if bits == 64 {
        0
    } else if minimum > 0 {
        // Lowered so its vlong is shorter. A writer that emits the true
        // minimum produces a longer, readable block — and one that differs
        // from Lucene's byte for byte.
        0.max(maximum.wrapping_sub(max_value(bits) as i64))
    } else {
        minimum
    };

    let token = (bits << BPV_SHIFT) | if minimum == 0 { MIN_VALUE_EQUALS_0 } else { 0 };
    output.write_byte(token as u8)?;

    if minimum != 0 {
        // The zero case is already carried by the token bit, so the encoded
        // range starts at one and the reader adds it back.
        output.write_vlong_signed(zig_zag_encode(minimum).wrapping_sub(1))?;
    }

    if bits > 0 {
        let shifted: Vec<u64> = values
            .iter()
            .map(|value| value.wrapping_sub(minimum) as u64)
            .collect();
        write_packed(output, &shifted, bits)?;
    }
    Ok(())
}

/// `MonotonicBlockPackedWriter`: one block of a non-decreasing stream.
///
/// The block carries its first value, the average as a **float**, and the
/// zigzag deltas from the line those two describe — or nothing at all when
/// every delta is zero, which is what a linear address stream produces and
/// is therefore the common case.
pub fn write_monotonic_block_packed<Sink: Write>(
    output: &mut CodecOutput<Sink>,
    values: &[i64],
) -> Result<()> {
    if values.is_empty() {
        return Err(crate::error::Error::InvalidFormat {
            details: "a monotonic block holds at least one value".to_owned(),
        });
    }

    // The block's *first* value, not its smallest.
    let minimum = values[0];
    let count = values.len();
    // The narrowing to `f32` is the format, not an approximation of it.
    // Lucene computes this average as a float precisely so that a writer and
    // a reader agree on the rounding, and writes its four bytes into the
    // block so neither has to re-derive it. Computing it in double here
    // would reconstruct different values on the reader's side.
    #[expect(
        clippy::cast_precision_loss,
        reason = "docs/analysis/lucene-4-7-codec.md §2.3: the average is a float by design"
    )]
    let average: f32 = if count == 1 {
        // Exactly `0f` for a single value, not the value itself.
        0.0
    } else {
        (values[count - 1].wrapping_sub(minimum)) as f32 / (count - 1) as f32
    };

    // The reader replays this arithmetic: a float multiply, then a
    // truncation toward zero. Computing it in double, or rounding instead of
    // truncating, reconstructs different values for large indices.
    let deltas: Vec<i64> = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            #[expect(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                reason = "the reader replays exactly this: a float multiply, then a \
                          truncation toward zero (§2.3)"
            )]
            let expected = (average * index as f32) as i64;
            zig_zag_encode(value.wrapping_sub(minimum).wrapping_sub(expected))
        })
        .collect();
    let widest = deltas.iter().copied().max().unwrap_or(0);

    output.write_vlong(minimum)?;
    output.write_int(average.to_bits() as i32)?;
    if widest == 0 {
        output.write_vint(0)?;
    } else {
        let bits = bits_required(widest as u64);
        output.write_vint(bits as i32)?;
        let encoded: Vec<u64> = deltas.iter().map(|delta| *delta as u64).collect();
        write_packed(output, &encoded, bits)?;
    }
    Ok(())
}
