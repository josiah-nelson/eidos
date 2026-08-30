//! Row-level Merkle anti-entropy for repair after log truncation.
//!
//! Production trees use 2^17–2^20 leaves. Only hashes are exchanged while
//! descending the tree; row images are requested for divergent leaves. An
//! empty authoritative leaf is meaningful and repairs stale local rows
//! without a filesystem crawl.

use eidos_domain::ObjectId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const MIN_FLEET_LEAF_BITS: u8 = 17;
pub const MAX_FLEET_LEAF_BITS: u8 = 20;

/// Leaf an object hashes into in a tree of `1 << leaf_bits` leaves. A pure
/// function of the object id, so peers agree without exchanging a tree.
pub fn leaf_index(leaf_bits: u8, object: ObjectId) -> u32 {
    let hash = blake3::hash(&object.raw().to_le_bytes());
    let first = u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("eight bytes"));
    (first & ((1u64 << leaf_bits) - 1)) as u32
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordDigest {
    pub object: ObjectId,
    pub generation: u64,
    pub content_hash: [u8; 32],
    pub deleted: bool,
}

impl RecordDigest {
    pub fn from_value(object: ObjectId, generation: u64, value: Option<&[u8]>) -> Self {
        let (content_hash, deleted) = match value {
            Some(bytes) => (*blake3::hash(bytes).as_bytes(), false),
            None => (*blake3::hash(b"eidos-tombstone/1").as_bytes(), true),
        };
        Self {
            object,
            generation,
            content_hash,
            deleted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MerkleError {
    #[error("fleet Merkle trees require {MIN_FLEET_LEAF_BITS}..={MAX_FLEET_LEAF_BITS} leaf bits, got {0}")]
    FleetLeafBits(u8),
    #[error("Merkle trees use different leaf counts ({left_bits} vs {right_bits} bits)")]
    Shape { left_bits: u8, right_bits: u8 },
    #[error("leaf {leaf} is outside a {leaf_count}-leaf Merkle tree")]
    Leaf { leaf: u32, leaf_count: usize },
}

#[derive(Debug, Clone)]
pub struct MerkleTree {
    leaf_bits: u8,
    leaf_count: usize,
    /// Binary heap with the root at 1 and leaves at `leaf_count..2*leaf_count`.
    nodes: Vec<[u8; 32]>,
    buckets: BTreeMap<u32, BTreeMap<ObjectId, RecordDigest>>,
}

impl MerkleTree {
    pub fn fleet(
        leaf_bits: u8,
        records: impl IntoIterator<Item = RecordDigest>,
    ) -> Result<Self, MerkleError> {
        if !(MIN_FLEET_LEAF_BITS..=MAX_FLEET_LEAF_BITS).contains(&leaf_bits) {
            return Err(MerkleError::FleetLeafBits(leaf_bits));
        }
        Ok(Self::with_leaf_bits(leaf_bits, records))
    }

    /// Small shapes are useful for exhaustive tests; fleet callers should use
    /// [`MerkleTree::fleet`] so the repair granularity stays in the researched
    /// 2^17–2^20 range.
    pub fn with_leaf_bits(leaf_bits: u8, records: impl IntoIterator<Item = RecordDigest>) -> Self {
        assert!(
            leaf_bits <= MAX_FLEET_LEAF_BITS,
            "Merkle shape is too large"
        );
        let leaf_count = 1usize << leaf_bits;
        let empty = Self::hash_leaf(std::iter::empty());
        let mut tree = Self {
            leaf_bits,
            leaf_count,
            nodes: vec![empty; leaf_count * 2],
            buckets: BTreeMap::new(),
        };
        for record in records {
            let leaf = tree.leaf_for(record.object);
            tree.buckets
                .entry(leaf)
                .or_default()
                .insert(record.object, record);
        }
        for leaf in 0..tree.leaf_count as u32 {
            tree.rehash_leaf(leaf);
        }
        tree.rebuild_parents();
        tree
    }

    pub fn leaf_bits(&self) -> u8 {
        self.leaf_bits
    }

    pub fn root(&self) -> [u8; 32] {
        self.nodes[1]
    }

    pub fn leaf_hashes(&self) -> Vec<[u8; 32]> {
        self.nodes[self.leaf_count..].to_vec()
    }

    pub fn leaf_for_object(&self, object: ObjectId) -> u32 {
        self.leaf_for(object)
    }

    pub fn upsert(&mut self, record: RecordDigest) {
        let leaf = self.leaf_for(record.object);
        self.buckets
            .entry(leaf)
            .or_default()
            .insert(record.object, record);
        self.rehash_path(leaf);
    }

    pub fn remove(&mut self, object: ObjectId) {
        let leaf = self.leaf_for(object);
        if let Some(bucket) = self.buckets.get_mut(&leaf) {
            bucket.remove(&object);
            if bucket.is_empty() {
                self.buckets.remove(&leaf);
            }
        }
        self.rehash_path(leaf);
    }

    /// Descend only unequal branches and return the row-transfer units.
    pub fn differing_leaves(&self, other: &Self) -> Result<Vec<u32>, MerkleError> {
        self.ensure_shape(other)?;
        let mut leaves = Vec::new();
        self.collect_differences(other, 1, &mut leaves);
        Ok(leaves)
    }

    pub fn records_in_leaf(&self, leaf: u32) -> Result<Vec<RecordDigest>, MerkleError> {
        if leaf as usize >= self.leaf_count {
            return Err(MerkleError::Leaf {
                leaf,
                leaf_count: self.leaf_count,
            });
        }
        Ok(self
            .buckets
            .get(&leaf)
            .map(|bucket| bucket.values().cloned().collect())
            .unwrap_or_default())
    }

    /// Replace divergent local leaves from an authoritative peer. Replacement
    /// rather than union is what makes deletion repair possible.
    pub fn repair_from(
        &mut self,
        authoritative: &Self,
        leaves: &[u32],
    ) -> Result<usize, MerkleError> {
        self.ensure_shape(authoritative)?;
        let mut changed_records = 0;
        for leaf in leaves.iter().copied().collect::<BTreeSet<_>>() {
            if leaf as usize >= self.leaf_count {
                return Err(MerkleError::Leaf {
                    leaf,
                    leaf_count: self.leaf_count,
                });
            }
            changed_records += self.buckets.get(&leaf).map(BTreeMap::len).unwrap_or(0);
            if let Some(rows) = authoritative.buckets.get(&leaf) {
                changed_records += rows.len();
                self.buckets.insert(leaf, rows.clone());
            } else {
                self.buckets.remove(&leaf);
            }
            self.rehash_path(leaf);
        }
        Ok(changed_records)
    }

    fn ensure_shape(&self, other: &Self) -> Result<(), MerkleError> {
        if self.leaf_bits != other.leaf_bits {
            return Err(MerkleError::Shape {
                left_bits: self.leaf_bits,
                right_bits: other.leaf_bits,
            });
        }
        Ok(())
    }

    fn leaf_for(&self, object: ObjectId) -> u32 {
        leaf_index(self.leaf_bits, object)
    }

    fn hash_leaf<'a>(records: impl IntoIterator<Item = &'a RecordDigest>) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"eidos-merkle-leaf/1");
        for record in records {
            hasher.update(&record.object.raw().to_le_bytes());
            hasher.update(&record.generation.to_le_bytes());
            hasher.update(&[u8::from(record.deleted)]);
            hasher.update(&record.content_hash);
        }
        *hasher.finalize().as_bytes()
    }

    fn hash_branch(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"eidos-merkle-branch/1");
        hasher.update(left);
        hasher.update(right);
        *hasher.finalize().as_bytes()
    }

    fn rehash_leaf(&mut self, leaf: u32) {
        self.nodes[self.leaf_count + leaf as usize] = self
            .buckets
            .get(&leaf)
            .map(|bucket| Self::hash_leaf(bucket.values()))
            .unwrap_or_else(|| Self::hash_leaf(std::iter::empty()));
    }

    fn rehash_path(&mut self, leaf: u32) {
        self.rehash_leaf(leaf);
        let mut node = (self.leaf_count + leaf as usize) / 2;
        while node > 0 {
            self.nodes[node] = Self::hash_branch(&self.nodes[node * 2], &self.nodes[node * 2 + 1]);
            node /= 2;
        }
    }

    fn rebuild_parents(&mut self) {
        for node in (1..self.leaf_count).rev() {
            self.nodes[node] = Self::hash_branch(&self.nodes[node * 2], &self.nodes[node * 2 + 1]);
        }
    }

    fn collect_differences(&self, other: &Self, node: usize, leaves: &mut Vec<u32>) {
        if self.nodes[node] == other.nodes[node] {
            return;
        }
        if node >= self.leaf_count {
            leaves.push((node - self.leaf_count) as u32);
            return;
        }
        self.collect_differences(other, node * 2, leaves);
        self.collect_differences(other, node * 2 + 1, leaves);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, generation: u64, value: Option<&[u8]>) -> RecordDigest {
        RecordDigest::from_value(ObjectId::new(id), generation, value)
    }

    #[test]
    fn a_single_divergence_descends_to_one_leaf_and_repairs() {
        let authoritative = MerkleTree::with_leaf_bits(
            5,
            [
                row(1, 1, Some(b"one")),
                row(2, 2, None),
                row(3, 1, Some(b"three")),
            ],
        );
        let mut stale = MerkleTree::with_leaf_bits(
            5,
            [
                row(1, 1, Some(b"one")),
                row(2, 1, Some(b"old")),
                row(3, 1, Some(b"three")),
            ],
        );
        let leaves = stale.differing_leaves(&authoritative).unwrap();
        assert_eq!(leaves.len(), 1);
        stale.repair_from(&authoritative, &leaves).unwrap();
        assert_eq!(stale.root(), authoritative.root());
    }

    #[test]
    fn authoritative_absence_removes_stale_rows() {
        let authoritative = MerkleTree::with_leaf_bits(4, []);
        let mut stale = MerkleTree::with_leaf_bits(4, [row(9, 1, Some(b"stale"))]);
        let leaves = stale.differing_leaves(&authoritative).unwrap();
        stale.repair_from(&authoritative, &leaves).unwrap();
        assert_eq!(stale.root(), authoritative.root());
    }

    #[test]
    fn fleet_shape_enforces_researched_granularity() {
        assert_eq!(
            MerkleTree::fleet(16, []).unwrap_err(),
            MerkleError::FleetLeafBits(16)
        );
        assert!(MerkleTree::fleet(MIN_FLEET_LEAF_BITS, []).is_ok());
    }
}
