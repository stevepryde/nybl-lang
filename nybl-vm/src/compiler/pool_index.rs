//! Compiler-only indexes for deterministic serialized chunk pools.
//!
//! Pool vectors remain authoritative and retain first-seen ordering. Default
//! builds use `HashMap` with its per-instance randomized state for expected
//! constant-time, hash-flood-resistant lookup. True `no_std` builds use
//! `BTreeMap`, preserving bounded logarithmic behavior without a dependency or
//! entropy source.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    collections::BTreeMap as IndexMap,
    string::{String, ToString},
    vec::Vec,
};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{collections::HashMap as IndexMap, string::String, vec::Vec};

use crate::chunk::{ConstIdx, Constant, NameIdx};

#[derive(Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ConstantKey {
    Int(i64),
    Number(u64),
    Str(String),
}

impl From<&Constant> for ConstantKey {
    fn from(constant: &Constant) -> Self {
        match constant {
            Constant::Int(value) => Self::Int(*value),
            Constant::Number(value) => Self::Number(value.to_bits()),
            Constant::Str(value) => Self::Str(value.clone()),
        }
    }
}

#[derive(Default)]
pub(super) struct ConstantPoolIndex {
    indices: IndexMap<ConstantKey, ConstIdx>,
}

impl ConstantPoolIndex {
    pub(super) fn intern(&mut self, pool: &mut Vec<Constant>, constant: Constant) -> ConstIdx {
        let key = ConstantKey::from(&constant);
        if let Some(index) = self.indices.get(&key) {
            return *index;
        }

        let index = ConstIdx(pool_index(pool.len(), "constant"));
        pool.push(constant);
        self.indices.insert(key, index);
        index
    }

    pub(super) fn len(&self) -> usize {
        self.indices.len()
    }
}

#[derive(Default)]
pub(super) struct NamePoolIndex {
    indices: IndexMap<String, NameIdx>,
}

impl NamePoolIndex {
    pub(super) fn intern(&mut self, pool: &mut Vec<String>, name: &str) -> NameIdx {
        if let Some(index) = self.indices.get(name) {
            return *index;
        }

        let name = name.to_string();
        let index = NameIdx(pool_index(pool.len(), "name"));
        pool.push(name.clone());
        self.indices.insert(name, index);
        index
    }

    pub(super) fn len(&self) -> usize {
        self.indices.len()
    }
}

fn pool_index(len: usize, pool_name: &str) -> u32 {
    u32::try_from(len)
        .unwrap_or_else(|_| panic!("compiler {pool_name} pool exceeds bytecode range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    use alloc::format;
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    use std::format;

    #[test]
    fn constants_keep_exact_keys_and_first_seen_indices() {
        let mut index = ConstantPoolIndex::default();
        let mut pool = Vec::new();
        let positive_zero = 0.0f64;
        let negative_zero = -0.0f64;
        let nan_a = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan_b = f64::from_bits(0x7ff8_0000_0000_0002);

        let values = [
            Constant::Int(1),
            Constant::Number(positive_zero),
            Constant::Number(negative_zero),
            Constant::Number(nan_a),
            Constant::Number(nan_b),
            Constant::Number(1.0),
            Constant::Str("1".to_string()),
        ];
        for (expected, value) in values.iter().cloned().enumerate() {
            assert_eq!(index.intern(&mut pool, value), ConstIdx(expected as u32));
        }
        for (expected, value) in values.iter().cloned().enumerate() {
            assert_eq!(index.intern(&mut pool, value), ConstIdx(expected as u32));
        }

        assert_eq!(pool.len(), values.len());
        let number_bits: Vec<u64> = pool
            .iter()
            .filter_map(|constant| match constant {
                Constant::Number(value) => Some(value.to_bits()),
                _ => None,
            })
            .collect();
        assert_eq!(
            number_bits,
            [
                positive_zero.to_bits(),
                negative_zero.to_bits(),
                nan_a.to_bits(),
                nan_b.to_bits(),
                1.0f64.to_bits(),
            ]
        );
    }

    #[test]
    fn names_keep_first_seen_indices() {
        let mut index = NamePoolIndex::default();
        let mut pool = Vec::new();

        assert_eq!(index.intern(&mut pool, "alpha"), NameIdx(0));
        assert_eq!(index.intern(&mut pool, "beta"), NameIdx(1));
        assert_eq!(index.intern(&mut pool, "alpha"), NameIdx(0));
        assert_eq!(index.intern(&mut pool, "gamma"), NameIdx(2));
        assert_eq!(index.intern(&mut pool, "beta"), NameIdx(1));
        assert_eq!(pool, ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn large_unique_and_repeated_inputs_use_one_index_entry_each() {
        const COUNT: usize = 8_192;
        let mut constants = ConstantPoolIndex::default();
        let mut constant_pool = Vec::new();
        let mut names = NamePoolIndex::default();
        let mut name_pool = Vec::new();

        for pass in 0..2 {
            for item in 0..COUNT {
                let expected = item as u32;
                let name = format!("generated_name_{item:05}");
                assert_eq!(names.intern(&mut name_pool, &name), NameIdx(expected));
                assert_eq!(
                    constants.intern(&mut constant_pool, Constant::Int(item as i64)),
                    ConstIdx(expected)
                );
            }
            assert_eq!(names.len(), COUNT, "name index grew on pass {pass}");
            assert_eq!(constants.len(), COUNT, "constant index grew on pass {pass}");
        }

        assert_eq!(name_pool.len(), COUNT);
        assert_eq!(constant_pool.len(), COUNT);
    }
}
