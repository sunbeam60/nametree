//! Name hashing. Every stored key depends on the hasher's output, so the shipped SipHash-1-3 is
//! implemented here and pinned by golden test vectors rather than taken from a dependency that
//! could change underneath us.

/// Turns a name into a 64-bit hash. The tree uses the top 59 bits as the name's key prefix.
///
/// The output for a given name must never change for the life of a stored tree.
pub trait NameHasher {
    /// Hashes the raw name bytes.
    fn hash(&self, name: &[u8]) -> u64;
}

impl<H: NameHasher + ?Sized> NameHasher for &H {
    fn hash(&self, name: &[u8]) -> u64 {
        (**self).hash(name)
    }
}

/// Keyed SipHash-1-3. With a random secret seed, crafting names that collide is infeasible.
///
/// The caller owns the seed. Yak generates a random 128-bit seed when it creates a file, stores
/// it in its header and builds the hasher from it on every open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SipHasher13 {
    k0: u64,
    k1: u64,
}

impl SipHasher13 {
    /// Builds the hasher from a 128-bit seed, read as two little-endian 64-bit keys.
    pub const fn new(seed: [u8; 16]) -> Self {
        let [a0, a1, a2, a3, a4, a5, a6, a7, b0, b1, b2, b3, b4, b5, b6, b7] = seed;
        Self::with_keys(
            u64::from_le_bytes([a0, a1, a2, a3, a4, a5, a6, a7]),
            u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]),
        )
    }

    /// Builds the hasher from the two 64-bit SipHash keys.
    pub const fn with_keys(k0: u64, k1: u64) -> Self {
        Self { k0, k1 }
    }
}

impl NameHasher for SipHasher13 {
    fn hash(&self, name: &[u8]) -> u64 {
        siphash::<1, 3>(self.k0, self.k1, name)
    }
}

/// SipHash with `C` compression rounds per block and `D` finalisation rounds.
fn siphash<const C: usize, const D: usize>(k0: u64, k1: u64, msg: &[u8]) -> u64 {
    // Initialisation constants from the SipHash paper: "somepseudorandomlygeneratedbytes".
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let compress = |v: &mut [u64; 4], m: u64| {
        v[3] ^= m;
        for _ in 0..C {
            sip_round(v);
        }
        v[0] ^= m;
    };

    let blocks = msg.chunks_exact(8);
    let tail = blocks.remainder();
    for block in blocks {
        let mut word = [0u8; 8];
        word.copy_from_slice(block);
        compress(&mut v, u64::from_le_bytes(word));
    }
    // Final block: leftover bytes, with the message length (mod 256) in the top byte.
    let mut last = (msg.len() as u64) << 56;
    for (i, &byte) in tail.iter().enumerate() {
        last |= u64::from(byte) << (8 * i);
    }
    compress(&mut v, last);

    // Finalisation constant from the SipHash paper.
    v[2] ^= 0xff;
    for _ in 0..D {
        sip_round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

fn sip_round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13) ^ v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16) ^ v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21) ^ v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17) ^ v[2];
    v[2] = v[2].rotate_left(32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::hash::Hasher;

    /// Key 00 01 .. 0f, as used by the SipHash paper's test vectors.
    const PAPER_SEED: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    /// Message of `len` bytes 00 01 02 .., as used by the SipHash paper's test vectors.
    fn paper_message(len: usize) -> Vec<u8> {
        (0..len as u8).collect()
    }

    /// The shared round function, checked against the reference SipHash-2-4 vectors.
    #[test]
    fn siphash24_matches_reference_vectors() {
        let h = SipHasher13::new(PAPER_SEED);
        let vectors: [(usize, u64); 4] = [
            (0, 0x726f_db47_dd0e_0e31),
            (1, 0x74f8_39c5_93dc_67fd),
            (2, 0x0d6c_8009_d9a9_4f5a),
            (63, 0x958a_324c_eb06_4572),
        ];
        for (len, expected) in vectors {
            assert_eq!(siphash::<2, 4>(h.k0, h.k1, &paper_message(len)), expected);
        }
    }

    #[test]
    fn siphash13_matches_reference_crate() {
        let ours = SipHasher13::new(PAPER_SEED);
        for len in 0..=MAX_TEST_LEN {
            let msg = paper_message(len);
            let mut reference = siphasher::sip::SipHasher13::new_with_key(&PAPER_SEED);
            reference.write(&msg);
            assert_eq!(ours.hash(&msg), reference.finish(), "length {len}");
        }
    }

    /// Longest message in the cross-check: covers every tail length several times over.
    const MAX_TEST_LEN: usize = 64;

    /// Golden vectors for the pinned hasher. If these change, every stored key changes.
    #[test]
    fn siphash13_golden_vectors() {
        let h = SipHasher13::new(PAPER_SEED);
        // Recorded from the `siphasher` crate's SipHash-1-3 with the same key.
        let golden: [(&[u8], u64); 4] = [
            (b"", 0xabac_0158_050f_c4dc),
            (b"a.txt", 0xe768_7b13_29c2_42f2),
            (b"docs", 0xea4a_c16a_27d8_3daf),
            (b"notes.md", 0x6f73_77c5_31af_95ed),
        ];
        for (name, expected) in golden {
            assert_eq!(h.hash(name), expected, "{:?}", core::str::from_utf8(name));
        }
    }
}
