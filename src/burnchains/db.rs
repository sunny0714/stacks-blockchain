// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2021 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::fmt;

use std::collections::{HashMap, HashSet};
use std::{cmp, fs, io, path::Path};

use rusqlite::{
    types::ToSql, Connection, OpenFlags, OptionalExtension, Row, Transaction, NO_PARAMS,
};
use serde_json;

use burnchains::affirmation::*;
use burnchains::Txid;
use burnchains::{
    Burnchain, BurnchainBlock, BurnchainBlockHeader, BurnchainSigner, Error as BurnchainError,
    PoxConstants,
};
use chainstate::burn::operations::{
    leader_block_commit::BURN_BLOCK_MINED_AT_MODULUS, BlockstackOperationType, LeaderBlockCommitOp,
};
use chainstate::burn::BlockSnapshot;
use chainstate::stacks::index::MarfTrieId;
use util::db::{
    query_row, query_row_panic, query_rows, sql_pragma, tx_begin_immediate, tx_busy_handler,
    u64_to_sql, DBConn, Error as DBError, FromColumn, FromRow,
};

use crate::types::chainstate::{BlockHeaderHash, BurnchainHeaderHash};
use crate::types::proof::ClarityMarfTrieId;

pub struct BurnchainDB {
    conn: Connection,
}

pub struct BurnchainDBTransaction<'a> {
    sql_tx: Transaction<'a>,
}

pub struct BurnchainBlockData {
    pub header: BurnchainBlockHeader,
    pub ops: Vec<BlockstackOperationType>,
}

/// A trait for reading burnchain block headers
pub trait BurnchainHeaderReader {
    fn read_burnchain_headers(
        &self,
        start_height: u64,
        end_height: u64,
    ) -> Result<Vec<BurnchainBlockHeader>, DBError>;
    fn get_burnchain_headers_height(&self) -> Result<u64, DBError>;
}

const NO_ANCHOR_BLOCK: u64 = i64::MAX as u64;

#[derive(Debug, Clone)]
pub struct BlockCommitMetadata {
    pub burn_block_hash: BurnchainHeaderHash,
    pub txid: Txid,
    pub block_height: u64,
    pub vtxindex: u32,
    pub affirmation_id: u64,
    /// if Some(..), then this block-commit is the anchor block for a reward cycle, and the
    /// reward cycle is represented as the inner u64.
    pub anchor_block: Option<u64>,
    /// If Some(..), then this is the anchor block that this block-commit descends from
    pub anchor_block_descendant: Option<u64>,
}

impl FromColumn<AffirmationMap> for AffirmationMap {
    fn from_column<'a>(row: &'a Row, col_name: &str) -> Result<AffirmationMap, DBError> {
        let txt: String = row.get_unwrap(col_name);
        let am = AffirmationMap::decode(&txt).ok_or(DBError::ParseError)?;
        Ok(am)
    }
}

impl FromRow<AffirmationMap> for AffirmationMap {
    fn from_row<'a>(row: &'a Row) -> Result<AffirmationMap, DBError> {
        AffirmationMap::from_column(row, "affirmation_map")
    }
}

impl FromRow<BlockCommitMetadata> for BlockCommitMetadata {
    fn from_row<'a>(row: &'a Row) -> Result<BlockCommitMetadata, DBError> {
        let burn_block_hash = BurnchainHeaderHash::from_column(row, "burn_block_hash")?;
        let txid = Txid::from_column(row, "txid")?;
        let block_height = u64::from_column(row, "block_height")?;
        let vtxindex: u32 = row.get_unwrap("vtxindex");
        let affirmation_id = u64::from_column(row, "affirmation_id")?;
        let anchor_block_u64 = u64::from_column(row, "anchor_block")?;
        let anchor_block = if anchor_block_u64 != NO_ANCHOR_BLOCK {
            Some(anchor_block_u64)
        } else {
            None
        };

        let anchor_block_descendant_u64 = u64::from_column(row, "anchor_block_descendant")?;
        let anchor_block_descendant = if anchor_block_descendant_u64 != NO_ANCHOR_BLOCK {
            Some(anchor_block_descendant_u64)
        } else {
            None
        };

        Ok(BlockCommitMetadata {
            burn_block_hash,
            txid,
            block_height,
            vtxindex,
            affirmation_id,
            anchor_block: anchor_block,
            anchor_block_descendant,
        })
    }
}

/// Apply safety checks on extracted blockstack transactions
/// - put them in order by vtxindex
/// - make sure there are no vtxindex duplicates
fn apply_blockstack_txs_safety_checks(
    block_height: u64,
    blockstack_txs: &mut Vec<BlockstackOperationType>,
) -> () {
    // safety -- make sure these are in order
    blockstack_txs.sort_by(|ref a, ref b| a.vtxindex().partial_cmp(&b.vtxindex()).unwrap());

    // safety -- no duplicate vtxindex (shouldn't happen but crash if so)
    if blockstack_txs.len() > 1 {
        for i in 0..blockstack_txs.len() - 1 {
            if blockstack_txs[i].vtxindex() == blockstack_txs[i + 1].vtxindex() {
                panic!(
                    "FATAL: BUG: duplicate vtxindex {} in block {}",
                    blockstack_txs[i].vtxindex(),
                    blockstack_txs[i].block_height()
                );
            }
        }
    }

    // safety -- block heights all match
    for tx in blockstack_txs.iter() {
        if tx.block_height() != block_height {
            panic!(
                "FATAL: BUG: block height mismatch: {} != {}",
                tx.block_height(),
                block_height
            );
        }
    }
}

impl FromRow<BurnchainBlockHeader> for BurnchainBlockHeader {
    fn from_row(row: &Row) -> Result<BurnchainBlockHeader, DBError> {
        let block_height = u64::from_column(row, "block_height")?;
        let block_hash = BurnchainHeaderHash::from_column(row, "block_hash")?;
        let timestamp = u64::from_column(row, "timestamp")?;
        let num_txs = u64::from_column(row, "num_txs")?;
        let parent_block_hash = BurnchainHeaderHash::from_column(row, "parent_block_hash")?;

        Ok(BurnchainBlockHeader {
            block_height,
            block_hash,
            timestamp,
            num_txs,
            parent_block_hash,
        })
    }
}

impl FromRow<BlockstackOperationType> for BlockstackOperationType {
    fn from_row(row: &Row) -> Result<BlockstackOperationType, DBError> {
        let serialized: String = row.get_unwrap("op");
        let deserialized = serde_json::from_str(&serialized)
            .expect("CORRUPTION: db store un-deserializable block op");

        Ok(deserialized)
    }
}

const BURNCHAIN_DB_SCHEMA: &'static str = r#"
CREATE TABLE burnchain_db_block_headers (
    block_height INTEGER NOT NULL,
    block_hash TEXT UNIQUE NOT NULL,
    parent_block_hash TEXT NOT NULL,
    num_txs INTEGER NOT NULL,
    timestamp INTEGER NOT NULL,

    PRIMARY KEY(block_hash)
);

CREATE TABLE burnchain_db_block_ops (
    block_hash TEXT NOT NULL,
    op TEXT NOT NULL,
    txid TEXT NOT NULL,

    FOREIGN KEY(block_hash) REFERENCES burnchain_db_block_headers(block_hash)
);

CREATE TABLE affirmation_maps (
    affirmation_id INTEGER PRIMARY KEY AUTOINCREMENT,
    weight INTEGER NOT NULL,
    affirmation_map TEXT NOT NULL
);

-- ensure anchor block uniqueness
CREATE TABLE anchor_blocks (
    reward_cycle INTEGER PRIMARY KEY  -- will be i64::MAX if absent
);

CREATE TABLE block_commit_metadata (
    burn_block_hash TEXT NOT NULL,
    txid TEXT NOT NULL,
    block_height INTEGER NOT NULL,
    vtxindex INTEGER NOT NULL,
    
    affirmation_id INTEGER NOT NULL,
    anchor_block INTEGER NOT NULL,
    anchor_block_descendant INTEGER NOT NULL,

    PRIMARY KEY(burn_block_hash,txid),
    FOREIGN KEY(affirmation_id) REFERENCES affirmation_maps(affirmation_id),
    FOREIGN KEY(anchor_block) REFERENCES anchor_blocks(reward_cycle)
);

-- override the canonical affirmation map at the operator's discression
CREATE TABLE overrides (
    reward_cycle INTEGER PRIMARY KEY NOT NULL,
    affirmation_map TEXT NOT NULL
);

INSERT INTO affirmation_maps(affirmation_id,weight,affirmation_map) VALUES (0,0,""); -- empty affirmation map
INSERT INTO anchor_blocks(reward_cycle) VALUES (9223372036854775807); -- non-existant reward cycle (i64::MAX)
"#;

impl<'a> BurnchainDBTransaction<'a> {
    fn store_burnchain_db_entry(
        &self,
        header: &BurnchainBlockHeader,
    ) -> Result<i64, BurnchainError> {
        let sql = "INSERT INTO burnchain_db_block_headers
                   (block_height, block_hash, parent_block_hash, num_txs, timestamp)
                   VALUES (?, ?, ?, ?, ?)";
        let args: &[&dyn ToSql] = &[
            &u64_to_sql(header.block_height)?,
            &header.block_hash,
            &header.parent_block_hash,
            &u64_to_sql(header.num_txs)?,
            &u64_to_sql(header.timestamp)?,
        ];
        match self.sql_tx.execute(sql, args) {
            Ok(_) => Ok(self.sql_tx.last_insert_rowid()),
            Err(e) => Err(BurnchainError::from(e)),
        }
    }

    fn insert_block_commit_affirmation_map(
        &self,
        affirmation_map: &AffirmationMap,
    ) -> Result<u64, DBError> {
        let weight = affirmation_map.weight();
        let sql = "INSERT INTO affirmation_maps (affirmation_map,weight) VALUES (?1,?2)";
        let args: &[&dyn ToSql] = &[&affirmation_map.encode(), &u64_to_sql(weight)?];
        match self.sql_tx.execute(sql, args) {
            Ok(_) => {
                let am_id = BurnchainDB::get_affirmation_map_id(&self.sql_tx, &affirmation_map)?
                    .expect("BUG: no affirmation ID for affirmation map we just inserted");
                Ok(am_id)
            }
            Err(e) => Err(DBError::SqliteError(e)),
        }
    }

    fn update_block_commit_affirmation(
        &self,
        block_commit: &LeaderBlockCommitOp,
        anchor_block_descendant: Option<u64>,
        affirmation_id: u64,
    ) -> Result<(), DBError> {
        let sql = "UPDATE block_commit_metadata SET affirmation_id = ?1, anchor_block_descendant = ?2 WHERE burn_block_hash = ?3 AND txid = ?4";
        let args: &[&dyn ToSql] = &[
            &u64_to_sql(affirmation_id)?,
            &u64_to_sql(anchor_block_descendant.unwrap_or(NO_ANCHOR_BLOCK))?,
            &block_commit.burn_header_hash,
            &block_commit.txid,
        ];
        match self.sql_tx.execute(sql, args) {
            Ok(_) => {
                test_debug!("Set affirmation map ID of {} - {},{},{} (parent {},{}) to {} (anchor block descendant? {:?})",
                            &block_commit.burn_header_hash, &block_commit.txid, block_commit.block_height, block_commit.vtxindex, block_commit.parent_block_ptr, block_commit.parent_vtxindex, affirmation_id, &anchor_block_descendant);
                Ok(())
            }
            Err(e) => Err(DBError::SqliteError(e)),
        }
    }

    pub fn set_anchor_block(
        &self,
        block_commit: &LeaderBlockCommitOp,
        target_reward_cycle: u64,
    ) -> Result<(), DBError> {
        let sql = "INSERT OR REPLACE INTO anchor_blocks (reward_cycle) VALUES (?1)";
        let args: &[&dyn ToSql] = &[&u64_to_sql(target_reward_cycle)?];
        self.sql_tx
            .execute(sql, args)
            .map_err(|e| DBError::SqliteError(e))?;

        let sql = "UPDATE block_commit_metadata SET anchor_block = ?1 WHERE burn_block_hash = ?2 AND txid = ?3";
        let args: &[&dyn ToSql] = &[
            &u64_to_sql(target_reward_cycle)?,
            &block_commit.burn_header_hash,
            &block_commit.txid,
        ];
        match self.sql_tx.execute(sql, args) {
            Ok(_) => {
                test_debug!(
                    "Set anchor block for reward cycle {} to {},{},{},{}",
                    target_reward_cycle,
                    &block_commit.burn_header_hash,
                    &block_commit.txid,
                    &block_commit.block_height,
                    &block_commit.vtxindex
                );
                Ok(())
            }
            Err(e) => Err(DBError::SqliteError(e)),
        }
    }

    pub fn clear_anchor_block(&self, reward_cycle: u64) -> Result<(), DBError> {
        let sql = "UPDATE block_commit_metadata SET anchor_block = ?1 WHERE anchor_block = ?2";
        let args: &[&dyn ToSql] = &[&u64_to_sql(NO_ANCHOR_BLOCK)?, &u64_to_sql(reward_cycle)?];
        self.sql_tx
            .execute(sql, args)
            .map(|_| ())
            .map_err(|e| DBError::SqliteError(e))
    }

    /// Clear the descendancy data and affirmations for all block-commits in a reward cycle
    /// (both the reward and prepare phases), as well as anchor block data.
    pub fn clear_reward_cycle_descendancies(
        &self,
        reward_cycle: u64,
        burnchain: &Burnchain,
    ) -> Result<(), DBError> {
        let first_block_height = burnchain.reward_cycle_to_block_height(reward_cycle);
        let last_block_height = burnchain.reward_cycle_to_block_height(reward_cycle + 1);

        test_debug!(
            "Clear descendancy data for reward cycle {} (blocks {}-{})",
            reward_cycle,
            first_block_height,
            last_block_height
        );

        let sql = "UPDATE block_commit_metadata SET affirmation_id = 0, anchor_block = ?1, anchor_block_descendant = ?2 WHERE block_height >= ?3 AND block_height < ?4";
        let args: &[&dyn ToSql] = &[
            &u64_to_sql(NO_ANCHOR_BLOCK)?,
            &u64_to_sql(NO_ANCHOR_BLOCK)?,
            &u64_to_sql(first_block_height)?,
            &u64_to_sql(last_block_height)?,
        ];
        self.sql_tx
            .execute(sql, args)
            .map(|_| ())
            .map_err(|e| DBError::SqliteError(e))
    }

    /// Calculate a burnchain block's block-commits' descendancy information
    pub fn update_block_descendancy<B: BurnchainHeaderReader>(
        &self,
        indexer: &B,
        hdr: &BurnchainBlockHeader,
        burnchain: &Burnchain,
    ) -> Result<(), BurnchainError> {
        // find all block-commits for this block
        let commits: Vec<LeaderBlockCommitOp> = {
            let block_ops_qry = "SELECT * FROM burnchain_db_block_ops WHERE block_hash = ?";
            let block_ops = query_rows(&self.sql_tx, block_ops_qry, &[&hdr.block_hash])?;
            block_ops
                .into_iter()
                .filter_map(|op| {
                    if let BlockstackOperationType::LeaderBlockCommit(opdata) = op {
                        Some(opdata)
                    } else {
                        None
                    }
                })
                .collect()
        };
        if commits.len() == 0 {
            test_debug!("No block-commits for block {}", hdr.block_height);
            return Ok(());
        }

        // for each commit[i], find its parent commit
        let mut parent_commits = vec![];
        for commit in commits.iter() {
            let parent_commit_opt = if commit.parent_block_ptr != 0 || commit.parent_vtxindex != 0 {
                // parent is not genesis
                BurnchainDB::get_commit_at(
                    &self.sql_tx,
                    indexer,
                    commit.parent_block_ptr,
                    commit.parent_vtxindex,
                )?
            } else {
                // parnet is genesis
                test_debug!(
                    "Parent block-commit of {},{},{} is the genesis commit",
                    &commit.txid,
                    commit.block_height,
                    commit.vtxindex
                );
                None
            };

            parent_commits.push(parent_commit_opt);
        }
        assert_eq!(parent_commits.len(), commits.len());

        // for each parent block-commit and block-commit, calculate the block-commit's new
        // affirmation map
        for (parent_commit_opt, commit) in parent_commits.iter().zip(commits.iter()) {
            if let Some(parent_commit) = parent_commit_opt.as_ref() {
                if get_parent_child_reward_cycles(parent_commit, commit, burnchain).is_some() {
                    // we have enough info to calculate this commit's affirmation
                    self.make_reward_phase_affirmation_map(burnchain, commit, parent_commit)?;
                } else {
                    // parent is invalid
                    test_debug!(
                        "No block-commit parent reward cycle found for {},{},{}",
                        &commit.txid,
                        commit.block_height,
                        commit.vtxindex
                    );
                    self.update_block_commit_affirmation(commit, None, 0)
                        .map_err(|e| BurnchainError::from(e))?;
                }
            } else {
                if commit.parent_block_ptr == 0 && commit.parent_vtxindex == 0 {
                    test_debug!(
                        "Block-commit parent of {},{},{} is genesis",
                        &commit.txid,
                        commit.block_height,
                        commit.vtxindex
                    );
                } else {
                    // this is an invalid commit -- no parent found
                    test_debug!(
                        "No block-commit parent found for {},{},{}",
                        &commit.txid,
                        commit.block_height,
                        commit.vtxindex
                    );
                }
                self.update_block_commit_affirmation(commit, None, 0)
                    .map_err(|e| BurnchainError::from(e))?;
            }
        }

        Ok(())
    }

    /// Update the anchor block descendancy information for the _reward_ phase of a reward cycle.
    /// That is, for each block-commit in this reward cycle, mark it as descending from this reward
    /// cycle's anchor block (if it exists), or not.  If there is no anchor block, then no block in
    /// this reward cycle descends from an anchor block.  Each reward-phase block-commit's affirmation
    /// map is updated by this method.
    /// Only call after the reward cycle's prepare phase's affirmation maps and descendancy information has been
    /// updated.
    pub fn update_reward_phase_descendancies<B: BurnchainHeaderReader>(
        &self,
        indexer: &B,
        reward_cycle: u64,
        burnchain: &Burnchain,
    ) -> Result<(), BurnchainError> {
        let first_block_height = burnchain.reward_cycle_to_block_height(reward_cycle);
        let last_block_height = burnchain.reward_cycle_to_block_height(reward_cycle + 1)
            - (burnchain.pox_constants.prepare_length as u64);
        let hdrs = indexer.read_burnchain_headers(first_block_height, last_block_height)?;
        let reward_phase_end =
            cmp::min(last_block_height, first_block_height + (hdrs.len() as u64));

        test_debug!(
            "Update reward-phase descendancies for reward cycle {} over {} headers between {}-{}",
            reward_cycle,
            hdrs.len(),
            first_block_height,
            reward_phase_end
        );

        for block_height in first_block_height..reward_phase_end {
            let hdr = &hdrs[(block_height - first_block_height) as usize];
            self.update_block_descendancy(indexer, hdr, burnchain)?;
        }

        test_debug!(
            "Updated reward-phase descendancies for reward cycle {}",
            reward_cycle
        );
        Ok(())
    }

    pub fn make_prepare_phase_affirmation_map<B: BurnchainHeaderReader>(
        &self,
        indexer: &B,
        burnchain: &Burnchain,
        reward_cycle: u64,
        block_commit: &LeaderBlockCommitOp,
        anchor_block: Option<&LeaderBlockCommitOp>,
        descends_from_anchor_block: bool,
    ) -> Result<u64, BurnchainError> {
        test_debug!(
            "Make affirmation map for {},{},{} (parent {},{}) in reward cycle {}",
            &block_commit.txid,
            block_commit.block_height,
            block_commit.vtxindex,
            block_commit.parent_block_ptr,
            block_commit.parent_vtxindex,
            reward_cycle
        );

        let parent = match BurnchainDB::get_commit_at(
            &self.sql_tx,
            indexer,
            block_commit.parent_block_ptr,
            block_commit.parent_vtxindex,
        )? {
            Some(p) => p,
            None => {
                if block_commit.parent_block_ptr == 0 && block_commit.vtxindex == 0 {
                    test_debug!(
                        "Prepare-phase commit {},{},{} builds off of genesis",
                        &block_commit.block_header_hash,
                        block_commit.block_height,
                        block_commit.vtxindex
                    );
                } else {
                    test_debug!(
                        "Prepare-phase commit {},{},{} has no parent, so must be invalid",
                        &block_commit.block_header_hash,
                        block_commit.block_height,
                        block_commit.vtxindex
                    );
                }
                return Ok(0);
            }
        };

        let parent_metadata =
            BurnchainDB::get_commit_metadata(&self.sql_tx, &parent.burn_header_hash, &parent.txid)?
                .expect("BUG: no metadata found for parent block-commit");

        let (am, affirmed_reward_cycle) = if let Some(ab) = anchor_block {
            let anchor_am_id = BurnchainDB::get_block_commit_affirmation_id(&self.sql_tx, &ab)?
                .expect("BUG: anchor block has no affirmation map");

            let mut am = BurnchainDB::get_affirmation_map(&self.sql_tx, anchor_am_id)
                .map_err(|e| BurnchainError::from(e))?
                .ok_or(BurnchainError::DBError(DBError::NotFoundError))?;

            if descends_from_anchor_block {
                test_debug!("Prepare-phase commit {},{},{} descends from anchor block {},{},{} for reward cycle {}", &block_commit.block_header_hash, block_commit.block_height, block_commit.vtxindex, &ab.block_header_hash, ab.block_height, ab.vtxindex, reward_cycle);
                am.push(AffirmationMapEntry::PoxAnchorBlockPresent);
                (am, Some(reward_cycle))
            } else {
                test_debug!("Prepare-phase commit {},{},{} does NOT descend from anchor block {},{},{} for reward cycle {}", &block_commit.block_header_hash, block_commit.block_height, block_commit.vtxindex, &ab.block_header_hash, ab.block_height, ab.vtxindex, reward_cycle);
                am.push(AffirmationMapEntry::PoxAnchorBlockAbsent);
                (am, parent_metadata.anchor_block_descendant)
            }
        } else {
            let (parent_reward_cycle, _) =
                get_parent_child_reward_cycles(&parent, block_commit, burnchain)
                    .ok_or(BurnchainError::DBError(DBError::NotFoundError))?;

            // load up the affirmation map for the last anchor block the parent affirmed
            let (mut am, parent_rc_opt) = match parent_metadata.anchor_block_descendant {
                Some(parent_ab_rc) => {
                    // parent affirmed some past anchor block
                    let (_, ab_metadata) = BurnchainDB::get_anchor_block_commit(&self.sql_tx, parent_ab_rc)?
                            .expect(&format!("BUG: parent descends from a reward cycle with an anchor block ({}), but no anchor block found", parent_ab_rc));

                    let mut am =
                        BurnchainDB::get_affirmation_map(&self.sql_tx, ab_metadata.affirmation_id)?
                            .expect("BUG: no affirmation map for parent commit's anchor block");

                    test_debug!("Prepare-phase commit {},{},{} does nothing for reward cycle {}, but it builds on its parent which affirms anchor block for reward cycle {} ({}) (affirms? {})",
                                    &block_commit.block_header_hash, block_commit.block_height, block_commit.vtxindex, reward_cycle, parent_ab_rc, &am, (am.len() as u64) < parent_ab_rc);

                    if (am.len() as u64) < parent_ab_rc {
                        // child is affirming the parent
                        am.push(AffirmationMapEntry::PoxAnchorBlockPresent);
                    }

                    (am, Some(parent_ab_rc))
                }
                None => {
                    let mut parent_am = BurnchainDB::get_affirmation_map(
                        &self.sql_tx,
                        parent_metadata.affirmation_id,
                    )?
                    .expect("BUG: no affirmation map for parent commit");

                    // parent affirms no anchor blocks
                    test_debug!("Prepare-phase commit {},{},{} does nothing for reward cycle {}, and it builds on a parent {},{} {} which affirms no anchor block (affirms? {})",
                                    &block_commit.block_header_hash, block_commit.block_height, block_commit.vtxindex, reward_cycle, block_commit.parent_block_ptr, block_commit.parent_vtxindex, &parent_am, (parent_am.len() as u64) < parent_reward_cycle);

                    if (parent_am.len() as u64) < parent_reward_cycle {
                        // child is affirming the parent
                        parent_am.push(AffirmationMapEntry::Nothing);
                    }

                    (parent_am, None)
                }
            };

            let num_affirmed = am.len() as u64;
            for rc in (num_affirmed + 1)..(reward_cycle + 1) {
                if BurnchainDB::has_anchor_block(&self.sql_tx, rc)? {
                    test_debug!(
                        "Commit {},{},{} skips reward cycle {} with anchor block",
                        &block_commit.block_header_hash,
                        block_commit.block_height,
                        block_commit.vtxindex,
                        rc
                    );
                    am.push(AffirmationMapEntry::PoxAnchorBlockAbsent);
                } else {
                    // affirmation weight increases even if there's no decision made, because
                    // the lack of a decision is still an affirmation of all prior decisions
                    test_debug!(
                        "Commit {},{},{} skips reward cycle {} without anchor block",
                        &block_commit.block_header_hash,
                        block_commit.block_height,
                        block_commit.vtxindex,
                        rc
                    );
                    am.push(AffirmationMapEntry::Nothing);
                }
            }

            test_debug!(
                "Prepare-phase commit {},{},{} affirms parent {},{} with {} descended from {:?}",
                &block_commit.block_header_hash,
                block_commit.block_height,
                block_commit.vtxindex,
                parent.block_height,
                parent.vtxindex,
                &am,
                &parent_metadata.anchor_block_descendant
            );

            (am, parent_rc_opt)
        };

        if let Some(am_id) = BurnchainDB::get_affirmation_map_id(&self.sql_tx, &am)
            .map_err(|e| BurnchainError::from(e))?
        {
            // child doesn't represent any new affirmations by the network, since its
            // affirmation map already exists.
            if cfg!(test) {
                let _am_weight = BurnchainDB::get_affirmation_weight(&self.sql_tx, am_id)?
                    .expect(&format!("BUG: no affirmation map {}", &am_id));

                test_debug!("Affirmation map of prepare-phase block-commit {},{},{} (parent {},{}) is old: {:?} weight {} affirmed {:?}",
                            &block_commit.txid, block_commit.block_height, block_commit.vtxindex, block_commit.parent_block_ptr, block_commit.parent_vtxindex, &am, _am_weight, &affirmed_reward_cycle);
            }

            self.update_block_commit_affirmation(block_commit, affirmed_reward_cycle, am_id)
                .map_err(|e| BurnchainError::from(e))?;
            Ok(am_id)
        } else {
            test_debug!("Affirmation map of prepare-phase block-commit {},{},{} (parent {},{}) is new: {:?} weight {} affirmed {:?}",
                        &block_commit.txid, block_commit.block_height, block_commit.vtxindex, block_commit.parent_block_ptr, block_commit.parent_vtxindex, &am, am.weight(), &affirmed_reward_cycle);

            let am_id = self
                .insert_block_commit_affirmation_map(&am)
                .map_err(|e| BurnchainError::from(e))?;
            self.update_block_commit_affirmation(block_commit, affirmed_reward_cycle, am_id)
                .map_err(|e| BurnchainError::from(e))?;
            Ok(am_id)
        }
    }

    fn make_reward_phase_affirmation_map(
        &self,
        burnchain: &Burnchain,
        block_commit: &LeaderBlockCommitOp,
        parent: &LeaderBlockCommitOp,
    ) -> Result<u64, BurnchainError> {
        assert_eq!(block_commit.parent_block_ptr as u64, parent.block_height);
        assert_eq!(block_commit.parent_vtxindex as u32, parent.vtxindex);

        let parent_metadata =
            BurnchainDB::get_commit_metadata(&self.sql_tx, &parent.burn_header_hash, &parent.txid)?
                .expect("BUG: no metadata found for existing block commit");

        test_debug!(
            "Reward-phase commit {},{},{} has parent {},{}, anchor block {:?}",
            &block_commit.block_header_hash,
            block_commit.block_height,
            block_commit.vtxindex,
            parent.block_height,
            parent.vtxindex,
            &parent_metadata.anchor_block_descendant
        );

        let child_reward_cycle = burnchain
            .block_height_to_reward_cycle(block_commit.block_height)
            .expect("BUG: block commit exists before first block height");

        let (am, affirmed_anchor_block_reward_cycle) =
            if let Some(parent_ab_rc) = parent_metadata.anchor_block_descendant {
                let am_id = parent_metadata.affirmation_id;
                let mut am = BurnchainDB::get_affirmation_map(&self.sql_tx, am_id)?
                    .expect("BUG: no affirmation map for parent commit");

                test_debug!("Affirmation map of parent is {}", &am);

                let start_rc = am.len() as u64;
                for rc in (start_rc + 1)..(child_reward_cycle + 1) {
                    if BurnchainDB::has_anchor_block(&self.sql_tx, rc)? {
                        test_debug!(
                            "Commit {},{},{} skips reward cycle {} with anchor block",
                            &block_commit.block_header_hash,
                            block_commit.block_height,
                            block_commit.vtxindex,
                            rc
                        );
                        am.push(AffirmationMapEntry::PoxAnchorBlockAbsent);
                    } else {
                        test_debug!(
                            "Commit {},{},{} skips reward cycle {} without anchor block",
                            &block_commit.block_header_hash,
                            block_commit.block_height,
                            block_commit.vtxindex,
                            rc
                        );
                        am.push(AffirmationMapEntry::Nothing);
                    }
                }

                (am, Some(parent_ab_rc))
            } else {
                let mut am = AffirmationMap::empty();
                for rc in 1..(child_reward_cycle + 1) {
                    if BurnchainDB::has_anchor_block(&self.sql_tx, rc)? {
                        test_debug!(
                            "Commit {},{},{} skips reward cycle {} with anchor block",
                            &block_commit.block_header_hash,
                            block_commit.block_height,
                            block_commit.vtxindex,
                            rc
                        );
                        am.push(AffirmationMapEntry::PoxAnchorBlockAbsent);
                    } else {
                        test_debug!(
                            "Commit {},{},{} skips reward cycle {} without anchor block",
                            &block_commit.block_header_hash,
                            block_commit.block_height,
                            block_commit.vtxindex,
                            rc
                        );
                        am.push(AffirmationMapEntry::Nothing);
                    }
                }
                (am, None)
            };

        if let Some(am_id) = BurnchainDB::get_affirmation_map_id(&self.sql_tx, &am)
            .map_err(|e| BurnchainError::from(e))?
        {
            // child doesn't represent any new affirmations by the network, since its
            // affirmation map already exists.
            if cfg!(test) {
                let _am_weight = BurnchainDB::get_affirmation_weight(&self.sql_tx, am_id)?
                    .expect(&format!("BUG: no affirmation map {}", &am_id));

                test_debug!("Affirmation map of reward-phase block-commit {},{},{} (parent {},{}) is old: {:?} weight {}",
                            &block_commit.txid, block_commit.block_height, block_commit.vtxindex, block_commit.parent_block_ptr, block_commit.parent_vtxindex, &am, _am_weight);
            }

            self.update_block_commit_affirmation(
                block_commit,
                affirmed_anchor_block_reward_cycle,
                am_id,
            )
            .map_err(|e| BurnchainError::from(e))?;
            Ok(am_id)
        } else {
            test_debug!("Affirmation map of reward-phase block-commit {},{},{} (parent {},{}) is new: {:?} weight {}",
                        &block_commit.txid, block_commit.block_height, block_commit.vtxindex, block_commit.parent_block_ptr, block_commit.parent_vtxindex, &am, am.weight());

            let am_id = self
                .insert_block_commit_affirmation_map(&am)
                .map_err(|e| BurnchainError::from(e))?;
            self.update_block_commit_affirmation(
                block_commit,
                affirmed_anchor_block_reward_cycle,
                am_id,
            )
            .map_err(|e| BurnchainError::from(e))?;
            Ok(am_id)
        }
    }

    fn insert_block_commit_metadata(&self, bcm: BlockCommitMetadata) -> Result<(), BurnchainError> {
        let commit_metadata_sql = "INSERT OR REPLACE INTO block_commit_metadata
                                   (burn_block_hash, txid, block_height, vtxindex, anchor_block, anchor_block_descendant, affirmation_id)
                                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
        let mut stmt = self.sql_tx.prepare(commit_metadata_sql)?;
        let args: &[&dyn ToSql] = &[
            &bcm.burn_block_hash,
            &bcm.txid,
            &u64_to_sql(bcm.block_height)?,
            &bcm.vtxindex,
            &u64_to_sql(bcm.anchor_block.unwrap_or(NO_ANCHOR_BLOCK))?,
            &u64_to_sql(bcm.anchor_block_descendant.unwrap_or(NO_ANCHOR_BLOCK))?,
            &u64_to_sql(bcm.affirmation_id)?,
        ];
        stmt.execute(args)?;
        Ok(())
    }

    fn store_blockstack_ops<B: BurnchainHeaderReader>(
        &self,
        burnchain: &Burnchain,
        indexer: &B,
        block_header: &BurnchainBlockHeader,
        block_ops: &[BlockstackOperationType],
    ) -> Result<(), BurnchainError> {
        let sql = "INSERT INTO burnchain_db_block_ops
                   (block_hash, txid, op) VALUES (?, ?, ?)";
        let mut stmt = self.sql_tx.prepare(sql)?;
        for op in block_ops.iter() {
            let serialized_op =
                serde_json::to_string(op).expect("Failed to serialize parsed BlockstackOp");
            let args: &[&dyn ToSql] = &[&block_header.block_hash, op.txid_ref(), &serialized_op];
            stmt.execute(args)?;
        }

        for op in block_ops.iter() {
            if let BlockstackOperationType::LeaderBlockCommit(ref opdata) = op {
                let bcm = BlockCommitMetadata {
                    burn_block_hash: block_header.block_hash.clone(),
                    txid: opdata.txid.clone(),
                    block_height: opdata.block_height,
                    vtxindex: opdata.vtxindex,
                    // NOTE: these fields are filled in by the subsequent call.
                    affirmation_id: 0,
                    anchor_block: None,
                    anchor_block_descendant: None,
                };
                self.insert_block_commit_metadata(bcm)?;
            }
        }

        self.update_block_descendancy(indexer, block_header, burnchain)?;
        Ok(())
    }

    pub fn commit(self) -> Result<(), BurnchainError> {
        self.sql_tx.commit().map_err(BurnchainError::from)
    }

    pub fn conn(&self) -> &DBConn {
        &self.sql_tx
    }

    pub fn get_canonical_chain_tip(&self) -> Result<BurnchainBlockHeader, BurnchainError> {
        let qry = "SELECT * FROM burnchain_db_block_headers ORDER BY block_height DESC, block_hash ASC LIMIT 1";
        let opt = query_row(&self.sql_tx, qry, NO_PARAMS)?;
        Ok(opt.expect("CORRUPTION: No canonical burnchain tip"))
    }

    /// You'd only do this in network emergencies, where node operators are expected to declare an
    /// anchor block missing (or present).  Ideally there'd be a smart contract somewhere for this.
    pub fn set_override_affirmation_map(
        &self,
        reward_cycle: u64,
        affirmation_map: AffirmationMap,
    ) -> Result<(), DBError> {
        assert_eq!((affirmation_map.len() as u64) + 1, reward_cycle);
        let qry = "INSERT INTO overrides (reward_cycle, affirmation_map) VALUES (?1, ?2)";
        let args: &[&dyn ToSql] = &[&u64_to_sql(reward_cycle)?, &affirmation_map.encode()];

        let mut stmt = self.sql_tx.prepare(qry)?;
        stmt.execute(args)?;
        Ok(())
    }

    pub fn clear_override_affirmation_map(&self, reward_cycle: u64) -> Result<(), DBError> {
        let qry = "DELETE FROM overrides WHERE reward_cycle = ?1";
        let args: &[&dyn ToSql] = &[&u64_to_sql(reward_cycle)?];

        let mut stmt = self.sql_tx.prepare(qry)?;
        stmt.execute(args)?;
        Ok(())
    }
}

impl BurnchainDB {
    pub fn connect(
        path: &str,
        burnchain: &Burnchain,
        readwrite: bool,
    ) -> Result<BurnchainDB, BurnchainError> {
        let mut create_flag = false;
        let open_flags = match fs::metadata(path) {
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    // need to create
                    if readwrite {
                        create_flag = true;
                        let ppath = Path::new(path);
                        let pparent_path = ppath
                            .parent()
                            .expect(&format!("BUG: no parent of '{}'", path));
                        fs::create_dir_all(&pparent_path)
                            .map_err(|e| BurnchainError::from(DBError::IOError(e)))?;

                        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                    } else {
                        return Err(BurnchainError::from(DBError::NoDBError));
                    }
                } else {
                    return Err(BurnchainError::from(DBError::IOError(e)));
                }
            }
            Ok(_md) => {
                // can just open
                if readwrite {
                    OpenFlags::SQLITE_OPEN_READ_WRITE
                } else {
                    OpenFlags::SQLITE_OPEN_READ_ONLY
                }
            }
        };

        let conn = Connection::open_with_flags(path, open_flags)
            .expect(&format!("FAILED to open: {}", path));

        conn.busy_handler(Some(tx_busy_handler))?;

        let mut db = BurnchainDB { conn };

        if create_flag {
            let db_tx = db.tx_begin()?;
            sql_pragma(&db_tx.sql_tx, "PRAGMA journal_mode = WAL;")?;
            db_tx.sql_tx.execute_batch(BURNCHAIN_DB_SCHEMA)?;

            let first_block_header = BurnchainBlockHeader {
                block_height: burnchain.first_block_height,
                block_hash: burnchain.first_block_hash.clone(),
                timestamp: burnchain.first_block_timestamp.into(),
                num_txs: 0,
                parent_block_hash: BurnchainHeaderHash::sentinel(),
            };

            db_tx.store_burnchain_db_entry(&first_block_header)?;

            let first_snapshot = BlockSnapshot::initial(
                burnchain.first_block_height,
                &burnchain.first_block_hash,
                burnchain.first_block_timestamp as u64,
            );
            let first_snapshot_commit_metadata = BlockCommitMetadata {
                burn_block_hash: first_snapshot.burn_header_hash.clone(),
                txid: first_snapshot.winning_block_txid.clone(),
                block_height: first_snapshot.block_height,
                vtxindex: 0,
                affirmation_id: 0,
                anchor_block: None,
                anchor_block_descendant: None,
            };
            db_tx.insert_block_commit_metadata(first_snapshot_commit_metadata)?;
            db_tx.commit()?;
        }

        Ok(db)
    }

    pub fn open(path: &str, readwrite: bool) -> Result<BurnchainDB, BurnchainError> {
        let open_flags = if readwrite {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let conn = Connection::open_with_flags(path, open_flags)?;
        conn.busy_handler(Some(tx_busy_handler))?;

        Ok(BurnchainDB { conn })
    }

    pub fn conn(&self) -> &DBConn {
        &self.conn
    }

    pub fn tx_begin<'a>(&'a mut self) -> Result<BurnchainDBTransaction<'a>, BurnchainError> {
        let sql_tx = tx_begin_immediate(&mut self.conn)?;
        Ok(BurnchainDBTransaction { sql_tx: sql_tx })
    }

    fn inner_get_canonical_chain_tip(
        conn: &DBConn,
    ) -> Result<BurnchainBlockHeader, BurnchainError> {
        let qry = "SELECT * FROM burnchain_db_block_headers ORDER BY block_height DESC, block_hash ASC LIMIT 1";
        let opt = query_row(conn, qry, NO_PARAMS)?;
        Ok(opt.expect("CORRUPTION: No canonical burnchain tip"))
    }

    pub fn get_canonical_chain_tip(&self) -> Result<BurnchainBlockHeader, BurnchainError> {
        BurnchainDB::inner_get_canonical_chain_tip(&self.conn)
    }

    #[cfg(test)]
    pub fn get_first_header(&self) -> Result<BurnchainBlockHeader, BurnchainError> {
        let qry = "SELECT * FROM burnchain_db_block_headers ORDER BY block_height ASC, block_hash DESC LIMIT 1";
        let opt = query_row(&self.conn, qry, NO_PARAMS)?;
        Ok(opt.expect("CORRUPTION: No canonical burnchain tip"))
    }

    pub fn get_burnchain_block(
        conn: &DBConn,
        block: &BurnchainHeaderHash,
    ) -> Result<BurnchainBlockData, BurnchainError> {
        let block_header_qry =
            "SELECT * FROM burnchain_db_block_headers WHERE block_hash = ? LIMIT 1";
        let block_ops_qry = "SELECT * FROM burnchain_db_block_ops WHERE block_hash = ?";

        let block_header = query_row(conn, block_header_qry, &[block])?
            .ok_or_else(|| BurnchainError::UnknownBlock(block.clone()))?;
        let block_ops = query_rows(conn, block_ops_qry, &[block])?;

        Ok(BurnchainBlockData {
            header: block_header,
            ops: block_ops,
        })
    }

    fn inner_get_burnchain_op(conn: &DBConn, txid: &Txid) -> Option<BlockstackOperationType> {
        let qry = "SELECT op FROM burnchain_db_block_ops WHERE txid = ?";

        match query_row(conn, qry, &[txid]) {
            Ok(res) => res,
            Err(e) => {
                warn!(
                    "BurnchainDB Error finding burnchain op: {:?}. txid = {}",
                    e, txid
                );
                None
            }
        }
    }

    pub fn get_burnchain_op(&self, txid: &Txid) -> Option<BlockstackOperationType> {
        BurnchainDB::inner_get_burnchain_op(&self.conn, txid)
    }

    /// Filter out the burnchain block's transactions that could be blockstack transactions.
    /// Return the ordered list of blockstack operations by vtxindex
    fn get_blockstack_transactions(
        &self,
        burnchain: &Burnchain,
        block: &BurnchainBlock,
        block_header: &BurnchainBlockHeader,
    ) -> Vec<BlockstackOperationType> {
        debug!(
            "Extract Blockstack transactions from block {} {}",
            block.block_height(),
            &block.block_hash()
        );

        let mut ops = Vec::new();
        let mut pre_stx_ops = HashMap::new();

        for tx in block.txs().iter() {
            let result =
                Burnchain::classify_transaction(burnchain, self, block_header, &tx, &pre_stx_ops);
            if let Some(classified_tx) = result {
                if let BlockstackOperationType::PreStx(pre_stx_op) = classified_tx {
                    pre_stx_ops.insert(pre_stx_op.txid.clone(), pre_stx_op);
                } else {
                    ops.push(classified_tx);
                }
            }
        }

        ops.extend(
            pre_stx_ops
                .into_iter()
                .map(|(_, op)| BlockstackOperationType::PreStx(op)),
        );

        ops.sort_by_key(|op| op.vtxindex());

        ops
    }

    pub fn get_affirmation_map(
        conn: &DBConn,
        affirmation_id: u64,
    ) -> Result<Option<AffirmationMap>, DBError> {
        let sql = "SELECT affirmation_map FROM affirmation_maps WHERE affirmation_id = ?1";
        let args: &[&dyn ToSql] = &[&u64_to_sql(affirmation_id)?];
        query_row(conn, sql, args)
    }

    pub fn get_affirmation_weight(
        conn: &DBConn,
        affirmation_id: u64,
    ) -> Result<Option<u64>, DBError> {
        let sql = "SELECT weight FROM affirmation_maps WHERE affirmation_id = ?1";
        let args: &[&dyn ToSql] = &[&u64_to_sql(affirmation_id)?];
        query_row(conn, sql, args)
    }

    pub fn get_affirmation_map_id(
        conn: &DBConn,
        affirmation_map: &AffirmationMap,
    ) -> Result<Option<u64>, DBError> {
        let sql = "SELECT affirmation_id FROM affirmation_maps WHERE affirmation_map = ?1";
        let args: &[&dyn ToSql] = &[&affirmation_map.encode()];
        query_row(conn, sql, args)
    }

    pub fn get_affirmation_map_id_at(
        conn: &DBConn,
        burn_header_hash: &BurnchainHeaderHash,
        txid: &Txid,
    ) -> Result<Option<u64>, DBError> {
        let sql = "SELECT affirmation_id FROM block_commit_metadata WHERE burn_block_hash = ?1 AND txid = ?2";
        let args: &[&dyn ToSql] = &[burn_header_hash, txid];
        query_row(conn, sql, args)
    }

    pub fn get_affirmation_map_at(
        conn: &DBConn,
        burn_header_hash: &BurnchainHeaderHash,
        txid: &Txid,
    ) -> Result<Option<AffirmationMap>, DBError> {
        let am_id_opt = BurnchainDB::get_affirmation_map_id_at(conn, burn_header_hash, txid)?;
        match am_id_opt {
            Some(am_id) => BurnchainDB::get_affirmation_map(conn, am_id),
            None => Ok(None),
        }
    }

    pub fn get_block_commit_affirmation_id(
        conn: &DBConn,
        block_commit: &LeaderBlockCommitOp,
    ) -> Result<Option<u64>, DBError> {
        BurnchainDB::get_affirmation_map_id_at(
            conn,
            &block_commit.burn_header_hash,
            &block_commit.txid,
        )
    }

    pub fn is_anchor_block(
        conn: &DBConn,
        burn_header_hash: &BurnchainHeaderHash,
        txid: &Txid,
    ) -> Result<bool, DBError> {
        let sql = "SELECT 1 FROM block_commit_metadata WHERE anchor_block != ?1 AND burn_block_hash = ?2 AND txid = ?3";
        let args: &[&dyn ToSql] = &[&u64_to_sql(NO_ANCHOR_BLOCK)?, burn_header_hash, txid];
        query_row(conn, sql, args)?.ok_or(DBError::NotFoundError)
    }

    pub fn has_anchor_block(conn: &DBConn, reward_cycle: u64) -> Result<bool, DBError> {
        let sql = "SELECT 1 FROM block_commit_metadata WHERE anchor_block = ?1";
        let args: &[&dyn ToSql] = &[&u64_to_sql(reward_cycle)?];
        Ok(query_row::<bool, _>(conn, sql, args)?.is_some())
    }

    pub fn get_anchor_block_commit(
        conn: &DBConn,
        reward_cycle: u64,
    ) -> Result<Option<(LeaderBlockCommitOp, BlockCommitMetadata)>, DBError> {
        if reward_cycle == NO_ANCHOR_BLOCK {
            return Ok(None);
        }

        let sql = "SELECT * FROM block_commit_metadata WHERE anchor_block = ?1";
        let args: &[&dyn ToSql] = &[&u64_to_sql(reward_cycle)?];
        let commit_metadata = match query_row::<BlockCommitMetadata, _>(conn, sql, args)? {
            Some(cmt) => cmt,
            None => {
                return Ok(None);
            }
        };

        let commit = BurnchainDB::get_block_commit(conn, &commit_metadata.txid)?
            .expect("BUG: no block-commit for block-commit metadata");

        Ok(Some((commit, commit_metadata)))
    }

    pub fn get_block_commit_affirmation_map(
        conn: &DBConn,
        block_commit: &LeaderBlockCommitOp,
    ) -> Result<Option<AffirmationMap>, DBError> {
        let am_id = match BurnchainDB::get_block_commit_affirmation_id(conn, block_commit)? {
            Some(am_id) => am_id,
            None => {
                return Ok(None);
            }
        };

        BurnchainDB::get_affirmation_map(conn, am_id)
    }

    // do NOT call directly; only use in tests
    pub fn store_new_burnchain_block_ops_unchecked<B: BurnchainHeaderReader>(
        &mut self,
        burnchain: &Burnchain,
        indexer: &B,
        block_header: &BurnchainBlockHeader,
        blockstack_ops: &Vec<BlockstackOperationType>,
    ) -> Result<(), BurnchainError> {
        let db_tx = self.tx_begin()?;

        test_debug!(
            "Store block {},{} with {} ops",
            &block_header.block_hash,
            block_header.block_height,
            blockstack_ops.len()
        );
        db_tx.store_burnchain_db_entry(block_header)?;
        db_tx.store_blockstack_ops(burnchain, indexer, &block_header, blockstack_ops)?;

        db_tx.commit()?;
        Ok(())
    }

    pub fn store_new_burnchain_block<B: BurnchainHeaderReader>(
        &mut self,
        burnchain: &Burnchain,
        indexer: &B,
        block: &BurnchainBlock,
    ) -> Result<Vec<BlockstackOperationType>, BurnchainError> {
        let header = block.header();
        debug!("Storing new burnchain block";
              "burn_header_hash" => %header.block_hash.to_string());
        let mut blockstack_ops = self.get_blockstack_transactions(burnchain, block, &header);
        apply_blockstack_txs_safety_checks(header.block_height, &mut blockstack_ops);

        self.store_new_burnchain_block_ops_unchecked(burnchain, indexer, &header, &blockstack_ops)?;
        Ok(blockstack_ops)
    }

    #[cfg(test)]
    pub fn raw_store_burnchain_block<B: BurnchainHeaderReader>(
        &mut self,
        burnchain: &Burnchain,
        indexer: &B,
        header: BurnchainBlockHeader,
        mut blockstack_ops: Vec<BlockstackOperationType>,
    ) -> Result<(), BurnchainError> {
        apply_blockstack_txs_safety_checks(header.block_height, &mut blockstack_ops);

        let db_tx = self.tx_begin()?;

        db_tx.store_burnchain_db_entry(&header)?;
        db_tx.store_blockstack_ops(burnchain, indexer, &header, &blockstack_ops)?;

        db_tx.commit()?;

        Ok(())
    }

    pub fn get_block_commit(
        conn: &DBConn,
        txid: &Txid,
    ) -> Result<Option<LeaderBlockCommitOp>, DBError> {
        let op = BurnchainDB::inner_get_burnchain_op(conn, txid);
        if let Some(BlockstackOperationType::LeaderBlockCommit(opdata)) = op {
            Ok(Some(opdata))
        } else {
            test_debug!("No block-commit tx {}", &txid);
            Ok(None)
        }
    }

    pub fn get_commit_in_block_at(
        conn: &DBConn,
        header_hash: &BurnchainHeaderHash,
        block_ptr: u32,
        vtxindex: u16,
    ) -> Result<Option<LeaderBlockCommitOp>, DBError> {
        let qry = "SELECT txid FROM block_commit_metadata WHERE block_height = ?1 AND vtxindex = ?2 AND burn_block_hash = ?3";
        let args: &[&dyn ToSql] = &[&block_ptr, &vtxindex, &header_hash];
        let txid = match query_row(&conn, qry, args) {
            Ok(Some(txid)) => txid,
            Ok(None) => {
                test_debug!(
                    "No block-commit metadata at block {}: {},{}",
                    &header_hash,
                    &block_ptr,
                    &vtxindex
                );
                return Ok(None);
            }
            Err(e) => {
                debug!(
                    "BurnchainDB Error {:?} finding PoX affirmation at {},{} in {:?}",
                    e, block_ptr, vtxindex, &header_hash
                );
                return Ok(None);
            }
        };

        BurnchainDB::get_block_commit(conn, &txid)
    }

    pub fn get_commit_at<B: BurnchainHeaderReader>(
        conn: &DBConn,
        indexer: &B,
        block_ptr: u32,
        vtxindex: u16,
    ) -> Result<Option<LeaderBlockCommitOp>, DBError> {
        let header_hash = match indexer
            .read_burnchain_headers(block_ptr as u64, (block_ptr + 1) as u64)?
            .first()
        {
            Some(hdr) => hdr.block_hash,
            None => {
                test_debug!("No headers at height {}", block_ptr);
                return Ok(None);
            }
        };

        BurnchainDB::get_commit_in_block_at(conn, &header_hash, block_ptr, vtxindex)
    }

    pub fn get_commit_metadata(
        conn: &DBConn,
        burn_block_hash: &BurnchainHeaderHash,
        txid: &Txid,
    ) -> Result<Option<BlockCommitMetadata>, DBError> {
        let args: &[&dyn ToSql] = &[burn_block_hash, txid];
        query_row_panic(
            conn,
            "SELECT * FROM block_commit_metadata WHERE burn_block_hash = ?1 AND txid = ?2",
            args,
            || {
                format!(
                    "BUG: more than one block-commit {},{}",
                    burn_block_hash, txid
                )
            },
        )
    }

    pub fn get_commit_metadata_at<B: BurnchainHeaderReader>(
        conn: &DBConn,
        indexer: &B,
        block_ptr: u32,
        vtxindex: u16,
    ) -> Result<Option<BlockCommitMetadata>, DBError> {
        let header_hash = match indexer
            .read_burnchain_headers(block_ptr as u64, (block_ptr + 1) as u64)?
            .first()
        {
            Some(hdr) => hdr.block_hash,
            None => {
                test_debug!("No headers at height {}", block_ptr);
                return Ok(None);
            }
        };

        let commit = BurnchainDB::get_commit_in_block_at(conn, &header_hash, block_ptr, vtxindex)?
            .expect(&format!(
                "BUG: no metadata for stored block-commit {},{},{})",
                &header_hash, block_ptr, vtxindex
            ));

        BurnchainDB::get_commit_metadata(conn, &header_hash, &commit.txid)
    }

    /// Get the block-commit and block metadata for the anchor block with the heaviest affirmation
    /// weight.
    pub fn get_heaviest_anchor_block(
        conn: &DBConn,
    ) -> Result<Option<(LeaderBlockCommitOp, BlockCommitMetadata)>, DBError> {
        match query_row::<BlockCommitMetadata, _>(
                        conn, "SELECT block_commit_metadata.* \
                               FROM affirmation_maps JOIN block_commit_metadata ON affirmation_maps.affirmation_id = block_commit_metadata.affirmation_id \
                               WHERE block_commit_metadata.anchor_block != ?1 \
                               ORDER BY affirmation_maps.weight DESC, block_commit_metadata.anchor_block DESC",
                        &[&u64_to_sql(NO_ANCHOR_BLOCK)?]
        )? {
            Some(metadata) => {
                let commit = BurnchainDB::get_block_commit(conn, &metadata.txid)?
                    .expect("BUG: no block commit for existing metadata");

                Ok(Some((commit, metadata)))
            }
            None => {
                test_debug!("No anchor block affirmations maps");
                Ok(None)
            }
        }
    }

    /// Find the affirmation map of the anchor block whose affirmation map is the heaviest.
    /// In the event of a tie, pick the one from the anchor block of the latest reward cycle.
    pub fn get_heaviest_anchor_block_affirmation_map(
        conn: &DBConn,
        burnchain: &Burnchain,
    ) -> Result<AffirmationMap, DBError> {
        match BurnchainDB::get_heaviest_anchor_block(conn)? {
            Some((_, metadata)) => {
                let last_reward_cycle = burnchain
                    .block_height_to_reward_cycle(metadata.block_height)
                    .unwrap_or(0)
                    + 1;

                // is there an override set for this reward cycle?
                if let Some(am) =
                    BurnchainDB::get_override_affirmation_map(conn, last_reward_cycle)?
                {
                    warn!(
                        "Overriding heaviest affirmation map for reward cycle {} to {}",
                        last_reward_cycle, &am
                    );
                    return Ok(am);
                }

                let am = BurnchainDB::get_affirmation_map(conn, metadata.affirmation_id)?.expect(
                    &format!(
                        "BUG: failed to load affirmation map {}",
                        metadata.affirmation_id
                    ),
                );

                if cfg!(test) {
                    let _weight =
                        BurnchainDB::get_affirmation_weight(conn, metadata.affirmation_id)?.expect(
                            &format!(
                                "BUG: have affirmation map {} but no weight",
                                &metadata.affirmation_id
                            ),
                        );

                    test_debug!(
                        "Heaviest anchor block affirmation map is {:?} (ID {}, weight {})",
                        &am,
                        metadata.affirmation_id,
                        _weight
                    );
                }
                Ok(am)
            }
            None => {
                test_debug!("No anchor block affirmations maps");
                Ok(AffirmationMap::empty())
            }
        }
    }

    /// Load an overridden affirmation map.
    /// You'd only do this in network emergencies, where node operators are expected to declare an
    /// anchor block missing (or present).  Ideally there'd be a smart contract somewhere for this.
    pub fn get_override_affirmation_map(
        conn: &DBConn,
        reward_cycle: u64,
    ) -> Result<Option<AffirmationMap>, DBError> {
        let am_opt: Option<AffirmationMap> = query_row_panic(
            conn,
            "SELECT affirmation_map FROM overrides WHERE reward_cycle = ?1",
            &[&u64_to_sql(reward_cycle)?],
            || format!("BUG: more than one override affirmation map for the same reward cycle"),
        )?;
        if let Some(am) = &am_opt {
            assert_eq!((am.len() + 1) as u64, reward_cycle);
        }
        Ok(am_opt)
    }

    /// Get the canonical affirmation map.  This is the heaviest anchor block affirmation map, but
    /// accounting for any subsequent reward cycles whose anchor blocks either aren't on the
    /// heaviest anchor block affirmation map, or which have no anchor blocks.
    pub fn get_canonical_affirmation_map<F>(
        conn: &DBConn,
        burnchain: &Burnchain,
        mut unconfirmed_oracle: F,
    ) -> Result<AffirmationMap, DBError>
    where
        F: FnMut(LeaderBlockCommitOp, BlockCommitMetadata) -> bool,
    {
        let canonical_tip =
            BurnchainDB::inner_get_canonical_chain_tip(conn).map_err(|e| match e {
                BurnchainError::DBError(dbe) => dbe,
                _ => DBError::Other(format!("Burnchain error: {:?}", &e)),
            })?;

        let last_reward_cycle = burnchain
            .block_height_to_reward_cycle(canonical_tip.block_height)
            .unwrap_or(0)
            + 1;

        // is there an override set for this reward cycle?
        if let Some(am) = BurnchainDB::get_override_affirmation_map(conn, last_reward_cycle)? {
            warn!(
                "Overriding heaviest affirmation map for reward cycle {} to {}",
                last_reward_cycle, &am
            );
            return Ok(am);
        }

        let mut heaviest_am =
            BurnchainDB::get_heaviest_anchor_block_affirmation_map(conn, burnchain)?;
        let start_rc = (heaviest_am.len() as u64) + 1;

        test_debug!(
            "Add reward cycles {}-{} to heaviest anchor block affirmation map {}",
            start_rc,
            last_reward_cycle,
            &heaviest_am
        );
        for rc in start_rc..last_reward_cycle {
            if let Some((commit, metadata)) = BurnchainDB::get_anchor_block_commit(conn, rc)? {
                let present = unconfirmed_oracle(commit, metadata);
                if present {
                    test_debug!("Assume present anchor block at {}", rc);
                    heaviest_am.push(AffirmationMapEntry::PoxAnchorBlockPresent);
                } else {
                    test_debug!("Assume absent anchor block at {}", rc);
                    heaviest_am.push(AffirmationMapEntry::PoxAnchorBlockAbsent);
                }
            } else {
                test_debug!("Assume no anchor block at {}", rc);
                heaviest_am.push(AffirmationMapEntry::Nothing);
            }
        }

        Ok(heaviest_am)
    }
}

#[cfg(test)]
pub mod tests {
    use std::convert::TryInto;

    use address::*;
    use burnchains::bitcoin::address::*;
    use burnchains::bitcoin::blocks::*;
    use burnchains::bitcoin::*;
    use burnchains::PoxConstants;
    use burnchains::BLOCKSTACK_MAGIC_MAINNET;
    use chainstate::burn::*;
    use chainstate::coordinator::tests::*;
    use chainstate::stacks::*;
    use deps::bitcoin::blockdata::transaction::Transaction as BtcTx;
    use deps::bitcoin::network::serialize::deserialize;
    use util::hash::*;

    use crate::types::chainstate::StacksAddress;
    use crate::types::chainstate::VRFSeed;

    use super::*;

    fn make_tx(hex_str: &str) -> BtcTx {
        let tx_bin = hex_bytes(hex_str).unwrap();
        deserialize(&tx_bin.to_vec()).unwrap()
    }

    impl BurnchainHeaderReader for Vec<BurnchainBlockHeader> {
        fn read_burnchain_headers(
            &self,
            start_height: u64,
            end_height: u64,
        ) -> Result<Vec<BurnchainBlockHeader>, DBError> {
            if start_height >= self.len() as u64 {
                return Ok(vec![]);
            }
            let end = cmp::min(end_height, self.len() as u64) as usize;
            Ok(self[(start_height as usize)..end].to_vec())
        }

        fn get_burnchain_headers_height(&self) -> Result<u64, DBError> {
            Ok(self.len() as u64)
        }
    }

    #[test]
    fn test_store_and_fetch() {
        let first_bhh = BurnchainHeaderHash([0; 32]);
        let first_timestamp = 321;
        let first_height = 1;

        let mut burnchain = Burnchain::regtest(":memory:");
        burnchain.pox_constants = PoxConstants::test_default();
        burnchain.pox_constants.sunset_start = 999;
        burnchain.pox_constants.sunset_end = 1000;

        burnchain.first_block_height = first_height;
        burnchain.first_block_hash = first_bhh.clone();
        burnchain.first_block_timestamp = first_timestamp;

        let mut burnchain_db = BurnchainDB::connect(":memory:", &burnchain, true).unwrap();

        let first_block_header = burnchain_db.get_canonical_chain_tip().unwrap();
        assert_eq!(&first_block_header.block_hash, &first_bhh);
        assert_eq!(&first_block_header.block_height, &first_height);
        assert_eq!(first_block_header.timestamp, first_timestamp as u64);
        assert_eq!(
            &first_block_header.parent_block_hash,
            &BurnchainHeaderHash::sentinel()
        );

        let headers = vec![first_block_header.clone()];
        let canon_hash = BurnchainHeaderHash([1; 32]);

        let canonical_block = BurnchainBlock::Bitcoin(BitcoinBlock::new(
            500,
            &canon_hash,
            &first_bhh,
            &vec![],
            485,
        ));
        let ops = burnchain_db
            .store_new_burnchain_block(&burnchain, &headers, &canonical_block)
            .unwrap();
        assert_eq!(ops.len(), 0);

        let vtxindex = 1;
        let noncanon_block_height = 400;
        let non_canon_hash = BurnchainHeaderHash([2; 32]);

        let fixtures = operations::leader_key_register::tests::get_test_fixtures(
            vtxindex,
            noncanon_block_height,
            non_canon_hash,
        );

        let parser = BitcoinBlockParser::new(BitcoinNetworkType::Testnet, BLOCKSTACK_MAGIC_MAINNET);
        let mut broadcast_ops = vec![];
        let mut expected_ops = vec![];

        for (ix, tx_fixture) in fixtures.iter().enumerate() {
            let tx = make_tx(&tx_fixture.txstr);
            let burnchain_tx = parser.parse_tx(&tx, ix + 1).unwrap();
            if let Some(res) = &tx_fixture.result {
                let mut res = res.clone();
                res.vtxindex = (ix + 1).try_into().unwrap();
                expected_ops.push(res.clone());
            }
            broadcast_ops.push(burnchain_tx);
        }

        let non_canonical_block = BurnchainBlock::Bitcoin(BitcoinBlock::new(
            400,
            &non_canon_hash,
            &first_bhh,
            &broadcast_ops,
            350,
        ));

        let ops = burnchain_db
            .store_new_burnchain_block(&burnchain, &headers, &non_canonical_block)
            .unwrap();
        assert_eq!(ops.len(), expected_ops.len());
        for op in ops.iter() {
            let expected_op = expected_ops
                .iter()
                .find(|candidate| candidate.txid == op.txid())
                .expect("FAILED to find parsed op in expected ops");
            if let BlockstackOperationType::LeaderKeyRegister(op) = op {
                assert_eq!(op, expected_op);
            } else {
                panic!("EXPECTED to parse a LeaderKeyRegister");
            }
        }

        let BurnchainBlockData { header, ops } =
            BurnchainDB::get_burnchain_block(&burnchain_db.conn, &non_canon_hash).unwrap();
        assert_eq!(ops.len(), expected_ops.len());
        for op in ops.iter() {
            let expected_op = expected_ops
                .iter()
                .find(|candidate| candidate.txid == op.txid())
                .expect("FAILED to find parsed op in expected ops");
            if let BlockstackOperationType::LeaderKeyRegister(op) = op {
                assert_eq!(op, expected_op);
            } else {
                panic!("EXPECTED to parse a LeaderKeyRegister");
            }
        }
        assert_eq!(&header, &non_canonical_block.header());

        let looked_up_canon = burnchain_db.get_canonical_chain_tip().unwrap();
        assert_eq!(&looked_up_canon, &canonical_block.header());

        let BurnchainBlockData { header, ops } =
            BurnchainDB::get_burnchain_block(&burnchain_db.conn, &canon_hash).unwrap();
        assert_eq!(ops.len(), 0);
        assert_eq!(&header, &looked_up_canon);
    }

    #[test]
    fn test_classify_stack_stx() {
        let first_bhh = BurnchainHeaderHash([0; 32]);
        let first_timestamp = 321;
        let first_height = 1;

        let mut burnchain = Burnchain::regtest(":memory:");
        burnchain.pox_constants = PoxConstants::test_default();
        burnchain.pox_constants.sunset_start = 999;
        burnchain.pox_constants.sunset_end = 1000;

        burnchain.first_block_height = first_height;
        burnchain.first_block_hash = first_bhh.clone();
        burnchain.first_block_timestamp = first_timestamp;

        let mut burnchain_db = BurnchainDB::connect(":memory:", &burnchain, true).unwrap();

        let first_block_header = burnchain_db.get_canonical_chain_tip().unwrap();
        assert_eq!(&first_block_header.block_hash, &first_bhh);
        assert_eq!(&first_block_header.block_height, &first_height);
        assert_eq!(first_block_header.timestamp, first_timestamp as u64);
        assert_eq!(
            &first_block_header.parent_block_hash,
            &BurnchainHeaderHash::sentinel()
        );

        let canon_hash = BurnchainHeaderHash([1; 32]);
        let mut headers = vec![first_block_header.clone()];

        let canonical_block = BurnchainBlock::Bitcoin(BitcoinBlock::new(
            500,
            &canon_hash,
            &first_bhh,
            &vec![],
            485,
        ));
        let ops = burnchain_db
            .store_new_burnchain_block(&burnchain, &headers, &canonical_block)
            .unwrap();
        assert_eq!(ops.len(), 0);

        // let's mine a block with a pre-stack-stx tx, and a stack-stx tx,
        //    the stack-stx tx should _fail_ to verify, because there's no
        //    corresponding pre-stack-stx.

        let parser = BitcoinBlockParser::new(BitcoinNetworkType::Testnet, BLOCKSTACK_MAGIC_MAINNET);

        let pre_stack_stx_0_txid = Txid([5; 32]);
        let pre_stack_stx_0 = BitcoinTransaction {
            txid: pre_stack_stx_0_txid.clone(),
            vtxindex: 0,
            opcode: Opcodes::PreStx as u8,
            data: vec![0; 80],
            data_amt: 0,
            inputs: vec![BitcoinTxInput {
                keys: vec![],
                num_required: 0,
                in_type: BitcoinInputType::Standard,
                tx_ref: (Txid([0; 32]), 1),
            }],
            outputs: vec![BitcoinTxOutput {
                units: 10,
                address: BitcoinAddress {
                    addrtype: BitcoinAddressType::PublicKeyHash,
                    network_id: BitcoinNetworkType::Mainnet,
                    bytes: Hash160([1; 20]),
                },
            }],
        };

        // this one will not have a corresponding pre_stack_stx tx.
        let stack_stx_0 = BitcoinTransaction {
            txid: Txid([4; 32]),
            vtxindex: 1,
            opcode: Opcodes::StackStx as u8,
            data: vec![1; 80],
            data_amt: 0,
            inputs: vec![BitcoinTxInput {
                keys: vec![],
                num_required: 0,
                in_type: BitcoinInputType::Standard,
                tx_ref: (Txid([0; 32]), 1),
            }],
            outputs: vec![BitcoinTxOutput {
                units: 10,
                address: BitcoinAddress {
                    addrtype: BitcoinAddressType::PublicKeyHash,
                    network_id: BitcoinNetworkType::Mainnet,
                    bytes: Hash160([1; 20]),
                },
            }],
        };

        // this one will have a corresponding pre_stack_stx tx.
        let stack_stx_0_second_attempt = BitcoinTransaction {
            txid: Txid([4; 32]),
            vtxindex: 2,
            opcode: Opcodes::StackStx as u8,
            data: vec![1; 80],
            data_amt: 0,
            inputs: vec![BitcoinTxInput {
                keys: vec![],
                num_required: 0,
                in_type: BitcoinInputType::Standard,
                tx_ref: (pre_stack_stx_0_txid.clone(), 1),
            }],
            outputs: vec![BitcoinTxOutput {
                units: 10,
                address: BitcoinAddress {
                    addrtype: BitcoinAddressType::PublicKeyHash,
                    network_id: BitcoinNetworkType::Mainnet,
                    bytes: Hash160([2; 20]),
                },
            }],
        };

        // this one won't have a corresponding pre_stack_stx tx.
        let stack_stx_1 = BitcoinTransaction {
            txid: Txid([3; 32]),
            vtxindex: 3,
            opcode: Opcodes::StackStx as u8,
            data: vec![1; 80],
            data_amt: 0,
            inputs: vec![BitcoinTxInput {
                keys: vec![],
                num_required: 0,
                in_type: BitcoinInputType::Standard,
                tx_ref: (Txid([0; 32]), 1),
            }],
            outputs: vec![BitcoinTxOutput {
                units: 10,
                address: BitcoinAddress {
                    addrtype: BitcoinAddressType::PublicKeyHash,
                    network_id: BitcoinNetworkType::Mainnet,
                    bytes: Hash160([1; 20]),
                },
            }],
        };

        // this one won't use the correct output
        let stack_stx_2 = BitcoinTransaction {
            txid: Txid([8; 32]),
            vtxindex: 4,
            opcode: Opcodes::StackStx as u8,
            data: vec![1; 80],
            data_amt: 0,
            inputs: vec![BitcoinTxInput {
                keys: vec![],
                num_required: 0,
                in_type: BitcoinInputType::Standard,
                tx_ref: (pre_stack_stx_0_txid.clone(), 2),
            }],
            outputs: vec![BitcoinTxOutput {
                units: 10,
                address: BitcoinAddress {
                    addrtype: BitcoinAddressType::PublicKeyHash,
                    network_id: BitcoinNetworkType::Mainnet,
                    bytes: Hash160([1; 20]),
                },
            }],
        };

        let ops_0 = vec![pre_stack_stx_0, stack_stx_0];

        let ops_1 = vec![stack_stx_1, stack_stx_0_second_attempt, stack_stx_2];

        let block_height_0 = 501;
        let block_hash_0 = BurnchainHeaderHash([2; 32]);
        let block_height_1 = 502;
        let block_hash_1 = BurnchainHeaderHash([3; 32]);

        let block_0 = BurnchainBlock::Bitcoin(BitcoinBlock::new(
            block_height_0,
            &block_hash_0,
            &first_bhh,
            &ops_0,
            350,
        ));

        headers.push(BurnchainBlockHeader {
            block_height: first_block_header.block_height + 1,
            block_hash: block_hash_0.clone(),
            parent_block_hash: first_bhh.clone(),
            num_txs: ops_0.len() as u64,
            timestamp: first_block_header.timestamp + 1,
        });

        let block_1 = BurnchainBlock::Bitcoin(BitcoinBlock::new(
            block_height_1,
            &block_hash_1,
            &block_hash_0,
            &ops_1,
            360,
        ));

        headers.push(BurnchainBlockHeader {
            block_height: first_block_header.block_height + 2,
            block_hash: block_hash_1.clone(),
            parent_block_hash: block_hash_0.clone(),
            num_txs: ops_1.len() as u64,
            timestamp: first_block_header.timestamp + 2,
        });

        let processed_ops_0 = burnchain_db
            .store_new_burnchain_block(&burnchain, &headers, &block_0)
            .unwrap();

        assert_eq!(
            processed_ops_0.len(),
            1,
            "Only pre_stack_stx op should have been accepted"
        );

        let processed_ops_1 = burnchain_db
            .store_new_burnchain_block(&burnchain, &headers, &block_1)
            .unwrap();

        assert_eq!(
            processed_ops_1.len(),
            1,
            "Only one stack_stx op should have been accepted"
        );

        let expected_pre_stack_addr = StacksAddress::from_bitcoin_address(&BitcoinAddress {
            addrtype: BitcoinAddressType::PublicKeyHash,
            network_id: BitcoinNetworkType::Mainnet,
            bytes: Hash160([1; 20]),
        });

        let expected_reward_addr = StacksAddress::from_bitcoin_address(&BitcoinAddress {
            addrtype: BitcoinAddressType::PublicKeyHash,
            network_id: BitcoinNetworkType::Mainnet,
            bytes: Hash160([2; 20]),
        });

        if let BlockstackOperationType::PreStx(op) = &processed_ops_0[0] {
            assert_eq!(&op.output, &expected_pre_stack_addr);
        } else {
            panic!("EXPECTED to parse a pre stack stx op");
        }

        if let BlockstackOperationType::StackStx(op) = &processed_ops_1[0] {
            assert_eq!(&op.sender, &expected_pre_stack_addr);
            assert_eq!(&op.reward_addr, &expected_reward_addr);
            assert_eq!(op.stacked_ustx, u128::from_be_bytes([1; 16]));
            assert_eq!(op.num_cycles, 1);
        } else {
            panic!("EXPECTED to parse a stack stx op");
        }
    }

    pub fn make_simple_block_commit(
        burnchain: &Burnchain,
        parent: Option<&LeaderBlockCommitOp>,
        burn_header: &BurnchainBlockHeader,
        block_hash: BlockHeaderHash,
    ) -> LeaderBlockCommitOp {
        let block_height = burn_header.block_height;
        let mut new_op = LeaderBlockCommitOp {
            sunset_burn: 0,
            block_header_hash: block_hash,
            new_seed: VRFSeed([1u8; 32]),
            parent_block_ptr: 0,
            parent_vtxindex: 0,
            key_block_ptr: 0,
            key_vtxindex: 0,
            memo: vec![0],

            commit_outs: vec![
                StacksAddress {
                    version: 26,
                    bytes: Hash160::empty(),
                },
                StacksAddress {
                    version: 26,
                    bytes: Hash160::empty(),
                },
            ],

            burn_fee: 10000,
            input: (next_txid(), 0),
            apparent_sender: BurnchainSigner {
                public_keys: vec![StacksPublicKey::from_hex(
                    "02d8015134d9db8178ac93acbc43170a2f20febba5087a5b0437058765ad5133d0",
                )
                .unwrap()],
                num_sigs: 1,
                hash_mode: AddressHashMode::SerializeP2PKH,
            },

            txid: next_txid(),
            vtxindex: 0,
            block_height: block_height,
            burn_parent_modulus: ((block_height - 1) % BURN_BLOCK_MINED_AT_MODULUS) as u8,
            burn_header_hash: burn_header.block_hash.clone(),
        };

        if burnchain.is_in_prepare_phase(block_height) {
            new_op.commit_outs = vec![StacksAddress {
                version: 26,
                bytes: Hash160::empty(),
            }];
        }

        if let Some(ref op) = parent {
            new_op.parent_block_ptr = op.block_height as u32;
            new_op.parent_vtxindex = op.vtxindex as u16;
        };

        new_op
    }

    #[test]
    fn test_get_commit_at() {
        let first_bhh = BurnchainHeaderHash([0; 32]);
        let first_timestamp = 0;
        let first_height = 1;

        let mut burnchain = Burnchain::regtest(":memory:");
        burnchain.pox_constants = PoxConstants::new(5, 3, 2, 3, 0, 99, 100);
        burnchain.first_block_height = first_height;
        burnchain.first_block_hash = first_bhh.clone();
        burnchain.first_block_timestamp = first_timestamp;

        let mut burnchain_db = BurnchainDB::connect(":memory:", &burnchain, true).unwrap();

        let first_block_header = burnchain_db.get_canonical_chain_tip().unwrap();

        let mut headers = vec![first_block_header.clone()];
        let mut parent = None;
        let mut parent_block_header: Option<BurnchainBlockHeader> = None;
        let mut cmts = vec![];

        for i in 0..5 {
            let hdr = BurnchainHeaderHash([(i + 1) as u8; 32]);
            let block_header = BurnchainBlockHeader {
                block_height: (first_height + i) as u64,
                block_hash: hdr,
                parent_block_hash: parent_block_header
                    .as_ref()
                    .map(|blk| blk.block_hash.clone())
                    .unwrap_or(first_block_header.block_hash.clone()),
                num_txs: 1,
                timestamp: i as u64,
            };

            headers.push(block_header.clone());
            parent_block_header = Some(block_header);
        }

        for i in 0..5 {
            let block_header = &headers[i + 1];

            let cmt = make_simple_block_commit(
                &burnchain,
                parent.as_ref(),
                block_header,
                BlockHeaderHash([((i + 1) as u8) | 0x80; 32]),
            );
            burnchain_db
                .store_new_burnchain_block_ops_unchecked(
                    &burnchain,
                    &headers,
                    block_header,
                    &vec![BlockstackOperationType::LeaderBlockCommit(cmt.clone())],
                )
                .unwrap();

            cmts.push(cmt.clone());
            parent = Some(cmt);
        }

        for i in 0..5 {
            let cmt = BurnchainDB::get_commit_at(
                &burnchain_db.conn(),
                &headers,
                (first_height + i) as u32,
                0,
            )
            .unwrap()
            .unwrap();
            assert_eq!(cmt, cmts[i as usize]);
        }

        let cmt = BurnchainDB::get_commit_at(&burnchain_db.conn(), &headers, 5, 0)
            .unwrap()
            .unwrap();
        assert_eq!(cmt, cmts[4]);

        // fork off the last stored commit block
        let fork_hdr = BurnchainHeaderHash([90 as u8; 32]);
        let fork_block_header = BurnchainBlockHeader {
            block_height: 4,
            block_hash: fork_hdr,
            parent_block_hash: BurnchainHeaderHash([5 as u8; 32]),
            num_txs: 0,
            timestamp: 4 as u64,
        };

        burnchain_db
            .store_new_burnchain_block_ops_unchecked(
                &burnchain,
                &headers,
                &fork_block_header,
                &vec![],
            )
            .unwrap();
        headers[4] = fork_block_header;

        let cmt = BurnchainDB::get_commit_at(&burnchain_db.conn(), &headers, 4, 0).unwrap();
        assert!(cmt.is_none());
    }
}
