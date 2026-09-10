use std::collections::{BTreeMap, BTreeSet};

/// A tag -> set-of-keys secondary index. Kept in sync with `Storage` by the
/// caller: every key inserted into `Storage` under some tags must also be
/// added here, under each of those tags, and every key removed from
/// `Storage` must be removed here from every tag it carried.
#[derive(Debug, Default)]
pub struct Index {
    by_tag: BTreeMap<String, BTreeSet<String>>,
}

impl Index {
    pub fn new() -> Self {
        Index {
            by_tag: BTreeMap::new(),
        }
    }

    /// Records that `key` carries `tag`.
    pub fn add(&mut self, tag: &str, key: &str) {
        self.by_tag
            .entry(tag.to_string())
            .or_default()
            .insert(key.to_string());
    }

    /// Removes `key` from `tag`'s key set. If the set becomes empty, the
    /// tag entry itself is dropped.
    pub fn remove(&mut self, tag: &str, key: &str) {
        if let Some(set) = self.by_tag.get_mut(tag) {
            set.remove(key);
            if set.is_empty() {
                self.by_tag.remove(tag);
            }
        }
    }

    /// All keys tagged with `tag`, in sorted order. Empty if the tag is
    /// unknown.
    pub fn keys_for_tag(&self, tag: &str) -> Vec<String> {
        self.by_tag
            .get(tag)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The number of distinct keys tagged with `tag`.
    pub fn count_for_tag(&self, tag: &str) -> usize {
        self.by_tag.get(tag).map(BTreeSet::len).unwrap_or(0)
    }
}
