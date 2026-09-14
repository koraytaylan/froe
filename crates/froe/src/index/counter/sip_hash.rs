//! Oak's SipHash-2-2 variant, the hash that makes the counter index
//! deterministic given its `seed`.
//!
//! `docs/analysis/index-property-storage.md` §9.2 quotes the Java and §9.1
//! the narrowing of the stored `seed` to 32 bits that every run after the one
//! which created it performs. §9.3 records the chain the counter editor
//! drives over this type, and that the hit test is on the *added child's own*
//! hash while the increment lands on every strict ancestor.
//!
//! This is not `SipHash` as published: Oak keeps four 64-bit state words,
//! seeds them from one 64-bit key rather than two, folds one message word per
//! step with no length padding and no finalization rounds, and reduces to 32
//! bits. Reproducing it exactly is the whole point — a hash that is
//! *better* would place the counter's samples at different paths than the
//! store already holds.
//!
//! All arithmetic is 64-bit and wrapping, and `Long.rotateLeft` masks its
//! distance to six bits, which `u64::rotate_left` also does.

/// One state of the chain: four 64-bit words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SipHash {
    first: u64,
    second: u64,
    third: u64,
    fourth: u64,
}

impl SipHash {
    /// The seeded initial state, which is the hash of the content root.
    ///
    /// Note what [`SipHash::hash_code`] does to it: the four words are
    /// `key_low ^ C0`, `key_high ^ C1`, `key_low ^ C2`, `key_high ^ C3`, and
    /// the fold exclusive-ors all four, so **both key halves cancel and the
    /// root's hash code is the same for every seed**. The seed only starts to
    /// matter one step down. A port checked only at the root would pass with
    /// the seeding wrong.
    #[must_use]
    pub const fn seeded(seed: i64) -> Self {
        let key_low = seed as u64;
        let key_high = (seed as u64).rotate_left(32);
        Self {
            first: key_low ^ 0x736f_6d65_7073_6575,
            second: key_high ^ 0x646f_7261_6e64_6f6d,
            third: key_low ^ 0x6c79_6765_6e65_7261,
            fourth: key_high ^ 0x7465_6462_7974_6573,
        }
    }

    /// One step down the tree: two `SipRound`s over the parent's state, then
    /// the message word folded into the first word.
    ///
    /// `message` is the Java string hash of the child's name, an `i32`
    /// widened with sign extension — which is what the call
    /// `new SipHash(parent, name.hashCode())` does.
    #[must_use]
    pub const fn extended(parent: Self, message: i64) -> Self {
        let mut first = parent.first;
        let mut second = parent.second;
        let mut third = parent.third;
        let mut fourth = parent.fourth;
        let mut round = 0;
        while round < 2 {
            first = first.wrapping_add(second);
            third = third.wrapping_add(fourth);
            second = second.rotate_left(13);
            fourth = fourth.rotate_left(16);
            second ^= first;
            fourth ^= third;
            first = first.rotate_left(32);
            third = third.wrapping_add(second);
            first = first.wrapping_add(fourth);
            second = second.rotate_left(17);
            fourth = fourth.rotate_left(21);
            second ^= third;
            fourth ^= first;
            third = third.rotate_left(32);
            round += 1;
        }
        first ^= message as u64;
        Self {
            first,
            second,
            third,
            fourth,
        }
    }

    /// The child's state, from its name.
    ///
    /// The name's hash is `java.lang.String::hashCode` over UTF-16 code
    /// units, which [`crate::hashing::utf16_string_hash`] is.
    #[must_use]
    pub fn for_child(self, child_name: &str) -> Self {
        Self::extended(
            self,
            i64::from(crate::hashing::utf16_string_hash(child_name)),
        )
    }

    /// The 32-bit value the counter's hit test masks.
    ///
    /// `x = v0 ^ v1 ^ v2 ^ v3`, then the low 32 bits of `x ^ (x >>> 16)` —
    /// an *unsigned* shift, and a narrowing cast that Java writes as
    /// `(int)`.
    #[must_use]
    pub const fn hash_code(self) -> i32 {
        let folded = self.first ^ self.second ^ self.third ^ self.fourth;
        (folded ^ (folded >> 16)) as i32
    }
}

/// The chained state at an absolute content path.
///
/// `/` is the seeded state; every element below it is one
/// [`SipHash::for_child`] step. This is the whole chain the counter editor
/// drives, so a rebuild computing a path's hash computes it here.
#[must_use]
pub fn hash_for_path(seed: i64, path: &str) -> SipHash {
    let mut hash = SipHash::seeded(seed);
    for element in path.split('/').filter(|element| !element.is_empty()) {
        hash = hash.for_child(element);
    }
    hash
}

/// The seed as every run after the one that created it uses it: narrowed to
/// 32 bits and sign-extended back.
///
/// `NodeCounterEditorProvider.getIndexEditor` reads a stored `seed` with
/// `s.getValue(Type.LONG).intValue()` into a `long` field, so only the low 32
/// bits take part. The run that *creates* the seed uses
/// `UUID.randomUUID().getMostSignificantBits()` untruncated, which is why the
/// stored value usually has high bits that never matter again.
#[must_use]
pub const fn narrowed_seed(stored_seed: i64) -> i64 {
    stored_seed as i32 as i64
}
