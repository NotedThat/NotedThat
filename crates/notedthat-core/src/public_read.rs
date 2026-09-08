use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A read operation that a knowledge base may expose without authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicReadCapability {
    /// Include the knowledge base in anonymous discovery responses.
    Discover,
    /// Allow anonymous directory and object-metadata listings.
    Browse,
    /// Allow anonymous reads of object content.
    Content,
    /// Allow anonymous search requests.
    Search,
}

/// The set of read capabilities exposed without authentication for one knowledge base.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicReadPolicy(BTreeSet<PublicReadCapability>);

/// Builds a policy from typed capabilities, deduplicating them into canonical order.
impl FromIterator<PublicReadCapability> for PublicReadPolicy {
    fn from_iter<T: IntoIterator<Item = PublicReadCapability>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl PublicReadPolicy {
    /// Returns whether no read capability is exposed without authentication.
    pub fn is_private(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns whether the given read capability is exposed without authentication.
    pub fn allows(&self, capability: PublicReadCapability) -> bool {
        self.0.contains(&capability)
    }
}
