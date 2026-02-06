use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MerkleError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Empty tree")]
    EmptyTree,
    #[error("Invalid proof")]
    InvalidProof,
    #[error("Node not found at level {0}, position {1}")]
    NodeNotFound(usize, usize),
}

/// A node in the Merkle tree
#[derive(Debug, Clone)]
pub struct MerkleNode {
    pub level: usize,
    pub position: usize,
    pub hash: Vec<u8>,
    pub left_child_pos: Option<usize>,
    pub right_child_pos: Option<usize>,
}

/// A proof that a leaf is part of the Merkle tree
#[derive(Debug, Clone)]
pub struct MerkleProof {
    pub leaf_position: usize,
    pub leaf_hash: Vec<u8>,
    /// Sibling hashes from leaf to root, with direction (true = right sibling)
    pub path: Vec<(Vec<u8>, bool)>,
    pub root_hash: Vec<u8>,
}

impl MerkleProof {
    /// Verify the proof
    pub fn verify(&self) -> bool {
        let mut current_hash = self.leaf_hash.clone();

        for (sibling_hash, is_right_sibling) in &self.path {
            let mut hasher = Sha256::new();
            if *is_right_sibling {
                hasher.update(&current_hash);
                hasher.update(sibling_hash);
            } else {
                hasher.update(sibling_hash);
                hasher.update(&current_hash);
            }
            current_hash = hasher.finalize().to_vec();
        }

        current_hash == self.root_hash
    }
}

/// Merkle tree implementation for tamper-evident audit trail
pub struct MerkleTree {
    conn: Connection,
}

impl MerkleTree {
    /// Create a new Merkle tree using the given connection
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// Get the underlying connection
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Build the Merkle tree from event hashes
    pub fn build(&mut self, leaf_hashes: &[Vec<u8>]) -> Result<Option<Vec<u8>>, MerkleError> {
        if leaf_hashes.is_empty() {
            return Ok(None);
        }

        // Clear existing tree
        self.conn.execute("DELETE FROM merkle_nodes", [])?;

        // Insert leaves (level 0)
        for (pos, hash) in leaf_hashes.iter().enumerate() {
            self.conn.execute(
                "INSERT INTO merkle_nodes (level, position, hash, left_child_pos, right_child_pos)
                 VALUES (?1, ?2, ?3, NULL, NULL)",
                params![0i64, pos as i64, hash.as_slice()],
            )?;
        }

        // Build tree bottom-up
        let mut level = 0usize;
        let mut num_nodes = leaf_hashes.len();

        while num_nodes > 1 {
            let next_level = level + 1;
            let mut next_pos = 0usize;

            let mut pos = 0usize;
            while pos < num_nodes {
                let left_hash = self.get_hash(level, pos)?;

                let (combined_hash, right_pos) = if pos + 1 < num_nodes {
                    let right_hash = self.get_hash(level, pos + 1)?;
                    let combined = hash_pair(&left_hash, &right_hash);
                    (combined, Some(pos + 1))
                } else {
                    // Odd node - promote it
                    (left_hash, None)
                };

                self.conn.execute(
                    "INSERT INTO merkle_nodes (level, position, hash, left_child_pos, right_child_pos)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        next_level as i64,
                        next_pos as i64,
                        combined_hash.as_slice(),
                        Some(pos as i64),
                        right_pos.map(|p| p as i64),
                    ],
                )?;

                next_pos += 1;
                pos += 2;
            }

            level = next_level;
            num_nodes = next_pos;
        }

        // Return root hash
        self.get_root()
    }

    /// Get the root hash
    pub fn get_root(&self) -> Result<Option<Vec<u8>>, MerkleError> {
        let max_level: Option<i64> =
            self.conn
                .query_row("SELECT MAX(level) FROM merkle_nodes", [], |row| row.get(0))?;

        match max_level {
            Some(level) => {
                let hash: Vec<u8> = self.conn.query_row(
                    "SELECT hash FROM merkle_nodes WHERE level = ?1 AND position = 0",
                    [level],
                    |row| row.get(0),
                )?;
                Ok(Some(hash))
            }
            None => Ok(None),
        }
    }

    /// Get hash at a specific position
    fn get_hash(&self, level: usize, position: usize) -> Result<Vec<u8>, MerkleError> {
        let hash: Vec<u8> = self
            .conn
            .query_row(
                "SELECT hash FROM merkle_nodes WHERE level = ?1 AND position = ?2",
                params![level as i64, position as i64],
                |row| row.get(0),
            )
            .map_err(|_| MerkleError::NodeNotFound(level, position))?;
        Ok(hash)
    }

    /// Generate a proof for a leaf at the given position
    pub fn generate_proof(&self, leaf_position: usize) -> Result<MerkleProof, MerkleError> {
        let root = self.get_root()?.ok_or(MerkleError::EmptyTree)?;
        let leaf_hash = self.get_hash(0, leaf_position)?;

        let max_level: i64 =
            self.conn
                .query_row("SELECT MAX(level) FROM merkle_nodes", [], |row| row.get(0))?;

        let mut path = Vec::new();
        let mut pos = leaf_position;

        for level in 0..max_level as usize {
            let sibling_pos = if pos % 2 == 0 { pos + 1 } else { pos - 1 };
            let is_right_sibling = pos % 2 == 0;

            // Try to get sibling hash (might not exist if odd number of nodes)
            if let Ok(sibling_hash) = self.get_hash(level, sibling_pos) {
                path.push((sibling_hash, is_right_sibling));
            }

            pos /= 2;
        }

        Ok(MerkleProof {
            leaf_position,
            leaf_hash,
            path,
            root_hash: root,
        })
    }

    /// Verify that a hash is in the tree
    pub fn verify(&self, leaf_position: usize, leaf_hash: &[u8]) -> Result<bool, MerkleError> {
        let stored_hash = self.get_hash(0, leaf_position)?;
        if stored_hash != leaf_hash {
            return Ok(false);
        }

        let proof = self.generate_proof(leaf_position)?;
        Ok(proof.verify())
    }

    /// Get the number of leaves in the tree
    pub fn leaf_count(&self) -> Result<usize, MerkleError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM merkle_nodes WHERE level = 0",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Get all nodes at a given level
    pub fn get_level(&self, level: usize) -> Result<Vec<MerkleNode>, MerkleError> {
        let mut stmt = self.conn.prepare(
            "SELECT level, position, hash, left_child_pos, right_child_pos
             FROM merkle_nodes WHERE level = ?1 ORDER BY position",
        )?;

        let nodes: Vec<MerkleNode> = stmt
            .query_map([level as i64], |row| {
                Ok(MerkleNode {
                    level: row.get::<_, i64>(0)? as usize,
                    position: row.get::<_, i64>(1)? as usize,
                    hash: row.get(2)?,
                    left_child_pos: row.get::<_, Option<i64>>(3)?.map(|p| p as usize),
                    right_child_pos: row.get::<_, Option<i64>>(4)?.map(|p| p as usize),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(nodes)
    }

    /// Get the height of the tree
    pub fn height(&self) -> Result<usize, MerkleError> {
        let max_level: Option<i64> =
            self.conn
                .query_row("SELECT MAX(level) FROM merkle_nodes", [], |row| row.get(0))?;
        Ok(max_level.map(|l| l as usize + 1).unwrap_or(0))
    }
}

/// Hash two nodes together
fn hash_pair(left: &[u8], right: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().to_vec()
}

/// Compute hash of a single value
pub fn hash_leaf(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrations::init_schema;

    fn setup_tree() -> MerkleTree {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        MerkleTree::new(conn)
    }

    #[test]
    fn test_build_tree_single_leaf() {
        let mut tree = setup_tree();
        let leaf = hash_leaf(b"event1");

        let root = tree.build(std::slice::from_ref(&leaf)).unwrap();

        assert!(root.is_some());
        assert_eq!(root.unwrap(), leaf);
        assert_eq!(tree.leaf_count().unwrap(), 1);
    }

    #[test]
    fn test_build_tree_two_leaves() {
        let mut tree = setup_tree();
        let leaf1 = hash_leaf(b"event1");
        let leaf2 = hash_leaf(b"event2");

        let root = tree.build(&[leaf1.clone(), leaf2.clone()]).unwrap();

        assert!(root.is_some());
        let expected_root = hash_pair(&leaf1, &leaf2);
        assert_eq!(root.unwrap(), expected_root);
        assert_eq!(tree.leaf_count().unwrap(), 2);
        assert_eq!(tree.height().unwrap(), 2);
    }

    #[test]
    fn test_build_tree_multiple_leaves() {
        let mut tree = setup_tree();
        let leaves: Vec<Vec<u8>> = (0..8)
            .map(|i| hash_leaf(format!("event{}", i).as_bytes()))
            .collect();

        let root = tree.build(&leaves).unwrap();

        assert!(root.is_some());
        assert_eq!(tree.leaf_count().unwrap(), 8);
        assert_eq!(tree.height().unwrap(), 4); // log2(8) + 1
    }

    #[test]
    fn test_build_tree_odd_leaves() {
        let mut tree = setup_tree();
        let leaves: Vec<Vec<u8>> = (0..5)
            .map(|i| hash_leaf(format!("event{}", i).as_bytes()))
            .collect();

        let root = tree.build(&leaves).unwrap();

        assert!(root.is_some());
        assert_eq!(tree.leaf_count().unwrap(), 5);
    }

    #[test]
    fn test_generate_and_verify_proof() {
        let mut tree = setup_tree();
        let leaves: Vec<Vec<u8>> = (0..8)
            .map(|i| hash_leaf(format!("event{}", i).as_bytes()))
            .collect();

        tree.build(&leaves).unwrap();

        // Verify each leaf
        for (pos, leaf) in leaves.iter().enumerate() {
            let proof = tree.generate_proof(pos).unwrap();
            assert!(proof.verify());
            assert_eq!(proof.leaf_hash, *leaf);
        }
    }

    #[test]
    fn test_verify_leaf() {
        let mut tree = setup_tree();
        let leaves: Vec<Vec<u8>> = (0..4)
            .map(|i| hash_leaf(format!("event{}", i).as_bytes()))
            .collect();

        tree.build(&leaves).unwrap();

        // Valid verification
        assert!(tree.verify(0, &leaves[0]).unwrap());
        assert!(tree.verify(2, &leaves[2]).unwrap());

        // Invalid hash
        let fake_hash = hash_leaf(b"fake");
        assert!(!tree.verify(0, &fake_hash).unwrap());
    }

    #[test]
    fn test_tamper_detection() {
        let mut tree = setup_tree();
        let leaves: Vec<Vec<u8>> = (0..4)
            .map(|i| hash_leaf(format!("event{}", i).as_bytes()))
            .collect();

        tree.build(&leaves).unwrap();
        let original_root = tree.get_root().unwrap().unwrap();

        // Tamper with a leaf
        tree.conn
            .execute(
                "UPDATE merkle_nodes SET hash = ?1 WHERE level = 0 AND position = 1",
                params![hash_leaf(b"tampered").as_slice()],
            )
            .unwrap();

        // Verification should fail
        assert!(!tree.verify(1, &leaves[1]).unwrap());

        // Root should still be the same (we only changed the leaf, not rebuilt)
        let current_root = tree.get_root().unwrap().unwrap();
        assert_eq!(current_root, original_root);
    }

    #[test]
    fn test_empty_tree() {
        let mut tree = setup_tree();
        let root = tree.build(&[]).unwrap();
        assert!(root.is_none());
        assert_eq!(tree.leaf_count().unwrap(), 0);
        assert_eq!(tree.height().unwrap(), 0);
    }

    #[test]
    fn test_proof_invalid_modification() {
        let mut tree = setup_tree();
        let leaves: Vec<Vec<u8>> = (0..4)
            .map(|i| hash_leaf(format!("event{}", i).as_bytes()))
            .collect();

        tree.build(&leaves).unwrap();
        let mut proof = tree.generate_proof(0).unwrap();

        // Modify the proof
        proof.leaf_hash = hash_leaf(b"modified");

        // Proof should no longer verify
        assert!(!proof.verify());
    }
}
