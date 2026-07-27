//! First-seen ordered free-variable collection for function compilation.
//!
//! Emission order comes exclusively from `ordered`; the membership index is a
//! compiler-only accelerator. Default builds use randomized hashing for
//! expected constant-time membership checks on untrusted identifiers. True
//! `no_std` builds use a dependency-free ordered set with logarithmic lookup.

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    collections::BTreeSet as NameSet,
    string::{String, ToString},
    vec::Vec,
};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::{collections::HashSet as NameSet, string::String, vec::Vec};

#[derive(Default)]
pub(super) struct FreeVariables {
    ordered: Vec<String>,
    membership: NameSet<String>,
    #[cfg(test)]
    membership_checks: usize,
}

impl FreeVariables {
    pub(super) fn record(&mut self, name: &str) {
        #[cfg(test)]
        {
            self.membership_checks += 1;
        }

        if self.membership.contains(name) {
            return;
        }

        let name = name.to_string();
        self.membership.insert(name.clone());
        self.ordered.push(name);
    }

    pub(super) fn into_ordered(self) -> Vec<String> {
        self.ordered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(feature = "no_std", not(feature = "std")))]
    use alloc::format;
    #[cfg(any(feature = "std", not(feature = "no_std")))]
    use std::format;

    #[test]
    fn repeated_and_unique_names_keep_first_seen_order() {
        let mut variables = FreeVariables::default();
        for name in ["zeta", "alpha", "zeta", "beta", "alpha"] {
            variables.record(name);
        }

        assert_eq!(variables.membership_checks, 5);
        assert_eq!(variables.into_ordered(), ["zeta", "alpha", "beta"]);
    }

    #[test]
    fn large_unique_and_repeated_input_uses_one_membership_check_per_candidate() {
        const COUNT: usize = 8_192;
        let mut variables = FreeVariables::default();

        for _ in 0..2 {
            for index in 0..COUNT {
                variables.record(&format!("unresolved_{index:05}"));
            }
        }

        assert_eq!(variables.membership_checks, COUNT * 2);
        let ordered = variables.into_ordered();
        assert_eq!(ordered.len(), COUNT);
        assert_eq!(
            ordered.first().map(String::as_str),
            Some("unresolved_00000")
        );
        assert_eq!(ordered.last().map(String::as_str), Some("unresolved_08191"));
    }
}
