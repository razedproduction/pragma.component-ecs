//! A hasher for maps keyed by `TypeId`s (DECISIONS #35).
//!
//! A `TypeId` is already a hash of its type, so SipHash's protection against chosen keys buys
//! nothing here and costs more than the rest of a warm spawn. The keys are type ids and lists of
//! them, never user data.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// A `HashMap` with [`TypeIdHasher`].
pub(crate) type TypeIdMap<K, V> = HashMap<K, V, BuildHasherDefault<TypeIdHasher>>;

/// Multiplicative mixing of 64-bit words (the FxHash step).
#[derive(Default)]
pub(crate) struct TypeIdHasher(u64);

/// An odd constant with well-spread bits (FxHash).
const SEED: u64 = 0x517c_c1b7_2722_0a95;

impl Hasher for TypeIdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(SEED);
    }

    fn write_u128(&mut self, word: u128) {
        // Truncation is the point: the low and the high half.
        self.write_u64(word as u64);
        self.write_u64(word.wrapping_shr(64) as u64);
    }

    fn write_usize(&mut self, word: usize) {
        self.write_u64(word as u64);
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;
    use std::hash::{BuildHasher, BuildHasherDefault};

    use super::*;

    #[test]
    fn distinct_types_hash_apart() {
        let build = BuildHasherDefault::<TypeIdHasher>::default();
        let ids = [
            TypeId::of::<u8>(),
            TypeId::of::<u16>(),
            TypeId::of::<(u8, u16)>(),
            TypeId::of::<String>(),
        ];
        let mut hashes: Vec<u64> = ids.iter().map(|id| build.hash_one(id)).collect();
        hashes.sort_unstable();
        hashes.dedup();
        assert_eq!(hashes.len(), ids.len());
        assert_ne!(
            build.hash_one([1_u8, 2].as_slice()),
            build.hash_one([2_u8, 1].as_slice())
        );
        assert_ne!(build.hash_one(1_u128), build.hash_one(1_u128 << 64));

        let mut map = TypeIdMap::default();
        map.insert(ids[0], 1);
        assert_eq!(map.get(&ids[0]), Some(&1));
        assert_eq!(map.get(&ids[1]), None);
    }
}
