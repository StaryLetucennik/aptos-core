// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    block_executor::BlockExecutor,
    components::chunk_output::ChunkOutput,
    db_bootstrapper::{generate_waypoint, maybe_bootstrap},
    mock_vm::{
        encode_mint_transaction, encode_reconfiguration_transaction, encode_transfer_transaction,
        MockVM, DISCARD_STATUS, KEEP_STATUS,
    },
};
use anyhow::Result;
use aptos_crypto::{ed25519::Ed25519PrivateKey, HashValue, PrivateKey, SigningKey, Uniform};
use aptos_db::AptosDB;
use aptos_executor_types::{
    BlockExecutorTrait, ExecutedChunk, LedgerUpdateOutput, TransactionReplayer, VerifyExecutionMode,
};
use aptos_state_view::StateViewId;
use aptos_storage_interface::{
    async_proof_fetcher::AsyncProofFetcher, DbReaderWriter, ExecutedTrees,
};
use aptos_types::{
    account_address::AccountAddress,
    aggregate_signature::AggregateSignature,
    block_executor::config::BlockExecutorConfigFromOnchain,
    block_info::BlockInfo,
    bytes::NumToBytes,
    chain_id::ChainId,
    ledger_info::{LedgerInfo, LedgerInfoWithSignatures},
    proof::definition::LeafCount,
    state_store::{state_key::StateKey, state_value::StateValue},
    test_helpers::transaction_test_helpers::{block, TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG},
    transaction::{
        signature_verified_transaction::SignatureVerifiedTransaction, ExecutionStatus,
        RawTransaction, Script, SignedTransaction, Transaction, TransactionListWithProof,
        TransactionOutput, TransactionPayload, TransactionStatus, Version,
    },
    write_set::{WriteOp, WriteSet, WriteSetMut},
};
use proptest::prelude::*;
use std::{iter::once, sync::Arc};

mod chunk_executor_tests;

fn execute_and_commit_block(
    executor: &TestExecutor,
    parent_block_id: HashValue,
    txn_index: u64,
) -> HashValue {
    let txn = encode_mint_transaction(gen_address(txn_index), 100);
    let id = gen_block_id(txn_index + 1);

    let output = executor
        .execute_block(
            (id, block(vec![txn])).into(),
            parent_block_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();
    let version = 2 * (txn_index + 1);
    assert_eq!(output.version(), version);

    let ledger_info = gen_ledger_info(version, output.root_hash(), id, txn_index + 1);
    executor.commit_blocks(vec![id], ledger_info).unwrap();
    id
}

struct TestExecutor {
    _path: aptos_temppath::TempPath,
    db: DbReaderWriter,
    executor: BlockExecutor<MockVM>,
}

impl TestExecutor {
    fn new() -> TestExecutor {
        let path = aptos_temppath::TempPath::new();
        path.create_as_dir().unwrap();
        let db = DbReaderWriter::new(AptosDB::new_for_test(path.path()));
        let genesis = aptos_vm_genesis::test_genesis_transaction();
        let waypoint = generate_waypoint::<MockVM>(&db, &genesis).unwrap();
        maybe_bootstrap::<MockVM>(&db, &genesis, waypoint).unwrap();
        let executor = BlockExecutor::new(db.clone());

        TestExecutor {
            _path: path,
            db,
            executor,
        }
    }
}

impl std::ops::Deref for TestExecutor {
    type Target = BlockExecutor<MockVM>;

    fn deref(&self) -> &Self::Target {
        &self.executor
    }
}

impl std::ops::DerefMut for TestExecutor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.executor
    }
}

fn gen_address(index: u64) -> AccountAddress {
    let bytes = index.to_be_bytes();
    let mut buf = [0; AccountAddress::LENGTH];
    buf[AccountAddress::LENGTH - 8..].copy_from_slice(&bytes);
    AccountAddress::new(buf)
}

fn gen_block_id(index: u64) -> HashValue {
    let bytes = index.to_be_bytes();
    let mut buf = [0; HashValue::LENGTH];
    buf[HashValue::LENGTH - 8..].copy_from_slice(&bytes);
    HashValue::new(buf)
}

fn gen_ledger_info(
    version: u64,
    root_hash: HashValue,
    commit_block_id: HashValue,
    timestamp_usecs: u64,
) -> LedgerInfoWithSignatures {
    let ledger_info = LedgerInfo::new(
        BlockInfo::new(
            0,
            0,
            commit_block_id,
            root_hash,
            version,
            timestamp_usecs,
            None,
        ),
        HashValue::zero(),
    );
    LedgerInfoWithSignatures::new(ledger_info, AggregateSignature::empty())
}

#[test]
#[cfg_attr(feature = "consensus-only-perf-test", ignore)]
fn test_executor_status() {
    let executor = TestExecutor::new();
    let parent_block_id = executor.committed_block_id();
    let block_id = gen_block_id(1);

    let txn0 = encode_mint_transaction(gen_address(0), 100);
    let txn1 = encode_mint_transaction(gen_address(1), 100);
    let txn2 = encode_transfer_transaction(gen_address(0), gen_address(1), 500);

    let output = executor
        .execute_block(
            (block_id, block(vec![txn0, txn1, txn2])).into(),
            parent_block_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();

    assert_eq!(
        &vec![
            KEEP_STATUS.clone(),
            KEEP_STATUS.clone(),
            DISCARD_STATUS.clone(),
        ],
        output.compute_status_for_input_txns()
    );
}

#[cfg(feature = "consensus-only-perf-test")]
#[test]
fn test_executor_status_consensus_only() {
    let executor = TestExecutor::new();
    let parent_block_id = executor.committed_block_id();
    let block_id = gen_block_id(1);

    let txn0 = encode_mint_transaction(gen_address(0), 100);
    let txn1 = encode_mint_transaction(gen_address(1), 100);
    let txn2 = encode_transfer_transaction(gen_address(0), gen_address(1), 500);

    let output = executor
        .execute_block(
            (block_id, block(vec![txn0, txn1, txn2])).into(),
            parent_block_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();

    // We should not discard any transactions because we don't actually execute them.
    assert_eq!(
        &vec![
            KEEP_STATUS.clone(),
            KEEP_STATUS.clone(),
            KEEP_STATUS.clone(),
        ],
        output.compute_status_for_input_txns()
    );
}

#[test]
fn test_executor_one_block() {
    let executor = TestExecutor::new();
    let parent_block_id = executor.committed_block_id();
    let block_id = gen_block_id(1);

    let num_user_txns = 100;
    let txns = (0..num_user_txns)
        .map(|i| encode_mint_transaction(gen_address(i), 100))
        .collect::<Vec<_>>();
    let output = executor
        .execute_block(
            (block_id, block(txns)).into(),
            parent_block_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();
    let version = num_user_txns + 1;
    assert_eq!(output.version(), version);
    let block_root_hash = output.root_hash();

    let ledger_info = gen_ledger_info(version, block_root_hash, block_id, 1);
    executor.commit_blocks(vec![block_id], ledger_info).unwrap();
}

#[test]
fn test_executor_multiple_blocks() {
    let executor = TestExecutor::new();
    let mut parent_block_id = executor.committed_block_id();

    for i in 0..100 {
        parent_block_id = execute_and_commit_block(&executor, parent_block_id, i);
    }
}

#[test]
#[cfg_attr(feature = "consensus-only-perf-test", ignore)]
fn test_executor_two_blocks_with_failed_txns() {
    let executor = TestExecutor::new();
    let parent_block_id = executor.committed_block_id();

    let block1_id = gen_block_id(1);
    let block2_id = gen_block_id(2);

    let block1_txns = (0..50)
        .map(|i| encode_mint_transaction(gen_address(i), 100))
        .collect::<Vec<_>>();
    let block2_txns = (0..50)
        .map(|i| {
            if i % 2 == 0 {
                encode_mint_transaction(gen_address(i + 50), 100)
            } else {
                encode_transfer_transaction(gen_address(i), gen_address(i + 1), 500)
            }
        })
        .collect::<Vec<_>>();
    let _output1 = executor
        .execute_block(
            (block1_id, block(block1_txns)).into(),
            parent_block_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();
    let output2 = executor
        .execute_block(
            (block2_id, block(block2_txns)).into(),
            block1_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();

    let ledger_info = gen_ledger_info(77, output2.root_hash(), block2_id, 1);
    executor
        .commit_blocks(vec![block1_id, block2_id], ledger_info)
        .unwrap();
}

#[test]
fn test_executor_commit_twice() {
    let executor = TestExecutor::new();
    let parent_block_id = executor.committed_block_id();
    let block1_txns = (0..5)
        .map(|i| encode_mint_transaction(gen_address(i), 100))
        .collect::<Vec<_>>();
    let block1_id = gen_block_id(1);
    let output1 = executor
        .execute_block(
            (block1_id, block(block1_txns)).into(),
            parent_block_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();
    let ledger_info = gen_ledger_info(6, output1.root_hash(), block1_id, 1);
    executor
        .commit_blocks(vec![block1_id], ledger_info.clone())
        .unwrap();
    // commit with the same ledger info again.
    executor
        .commit_blocks(vec![block1_id], ledger_info)
        .unwrap();
}

#[test]
fn test_executor_execute_same_block_multiple_times() {
    let executor = TestExecutor::new();
    let parent_block_id = executor.committed_block_id();
    let block_id = gen_block_id(1);
    let version = 100;

    let txns: Vec<_> = (0..version)
        .map(|i| encode_mint_transaction(gen_address(i), 100))
        .collect();

    let mut responses = vec![];
    for _i in 0..100 {
        let output = executor
            .execute_block(
                (block_id, block(txns.clone())).into(),
                parent_block_id,
                TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
            )
            .unwrap();
        responses.push(output);
    }
    responses.dedup();
    assert_eq!(responses.len(), 1);
}

/// Generates a list of `TransactionListWithProof`s according to the given ranges.
fn create_transaction_chunks(
    chunk_ranges: Vec<std::ops::Range<Version>>,
) -> (Vec<TransactionListWithProof>, LedgerInfoWithSignatures) {
    assert_eq!(chunk_ranges.first().unwrap().start, 1);
    for i in 1..chunk_ranges.len() {
        let previous_range = &chunk_ranges[i - 1];
        let range = &chunk_ranges[i];
        assert!(previous_range.start <= previous_range.end);
        assert!(range.start <= range.end);
        assert!(range.start <= previous_range.end);
        assert!(previous_range.end <= range.end);
    }

    // To obtain the batches of transactions, we first execute and save all these transactions in a
    // separate DB. Then we call get_transactions to retrieve them.
    let TestExecutor { executor, .. } = TestExecutor::new();

    let mut txns: Vec<SignatureVerifiedTransaction> = vec![];
    for i in 1..(chunk_ranges.last().unwrap().end - 1) {
        let txn = encode_mint_transaction(gen_address(i), 100);
        txns.push(txn.into());
    }

    let id = gen_block_id(1);

    let output = executor
        .execute_block(
            (id, txns.clone()).into(),
            executor.committed_block_id(),
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();

    let ledger_version = txns.len() as u64 + 1;
    assert_eq!(ledger_version, output.transactions_to_commit_len() as u64);
    let ledger_info = gen_ledger_info(ledger_version, output.root_hash(), id, 1);
    executor
        .commit_blocks(vec![id], ledger_info.clone())
        .unwrap();

    let batches: Vec<_> = chunk_ranges
        .into_iter()
        .map(|range| {
            executor
                .db
                .reader
                .get_transactions(
                    range.start,
                    range.end - range.start,
                    ledger_version,
                    false, /* fetch_events */
                )
                .unwrap()
        })
        .collect();

    (batches, ledger_info)
}

#[test]
#[cfg_attr(feature = "consensus-only-perf-test", ignore)]
fn test_noop_block_after_reconfiguration() {
    let executor = TestExecutor::new();
    let mut parent_block_id = executor.committed_block_id();
    let first_txn = encode_reconfiguration_transaction();
    let first_block_id = gen_block_id(1);
    let output1 = executor
        .execute_block(
            (first_block_id, vec![first_txn]).into(),
            parent_block_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();
    parent_block_id = first_block_id;
    let second_block = TestBlock::new(10, 10, gen_block_id(2));
    let output2 = executor
        .execute_block(
            (second_block.id, second_block.txns).into(),
            parent_block_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();
    assert_eq!(output1.root_hash(), output2.root_hash());
}

fn create_test_transaction(sequence_number: u64) -> Transaction {
    let private_key = Ed25519PrivateKey::generate_for_testing();
    let public_key = private_key.public_key();

    let transaction_payload = TransactionPayload::Script(Script::new(vec![], vec![], vec![]));
    let raw_transaction = RawTransaction::new(
        AccountAddress::random(),
        sequence_number,
        transaction_payload,
        0,
        0,
        0,
        ChainId::new(10),
    );
    let signed_transaction = SignedTransaction::new(
        raw_transaction.clone(),
        public_key,
        private_key.sign(&raw_transaction).unwrap(),
    );

    Transaction::UserTransaction(signed_transaction)
}

fn apply_transaction_by_writeset(
    db: &DbReaderWriter,
    transactions_and_writesets: Vec<(Transaction, WriteSet)>,
) {
    let ledger_view: ExecutedTrees = db.reader.get_latest_executed_trees().unwrap();

    let transactions_and_outputs = transactions_and_writesets
        .iter()
        .map(|(txn, write_set)| {
            (
                txn.clone(),
                TransactionOutput::new(
                    write_set.clone(),
                    vec![],
                    0,
                    TransactionStatus::Keep(ExecutionStatus::MiscellaneousError(None)),
                ),
            )
        })
        .chain(once((
            Transaction::StateCheckpoint(HashValue::random()),
            TransactionOutput::new(
                WriteSet::default(),
                Vec::new(),
                0,
                TransactionStatus::Keep(ExecutionStatus::Success),
            ),
        )))
        .collect();

    let state_view = ledger_view
        .verified_state_view(
            StateViewId::Miscellaneous,
            Arc::clone(&db.reader),
            Arc::new(AsyncProofFetcher::new(db.reader.clone())),
        )
        .unwrap();

    let chunk_output =
        ChunkOutput::by_transaction_output(transactions_and_outputs, state_view).unwrap();

    let (executed, _, _) = chunk_output.apply_to_ledger(&ledger_view, None).unwrap();
    let ExecutedChunk {
        result_state,
        ledger_info,
        next_epoch_state: _,
        ledger_update_output,
    } = executed;
    let LedgerUpdateOutput {
        statuses_for_input_txns: _,
        to_commit,
        reconfig_events: _,
        transaction_info_hashes: _,
        state_updates_until_last_checkpoint: state_updates_before_last_checkpoint,
        sharded_state_cache,
        transaction_accumulator: _,
    } = ledger_update_output;

    db.writer
        .save_transactions(
            &to_commit,
            ledger_view.txn_accumulator().num_leaves(),
            ledger_view.state().base_version,
            ledger_info.as_ref(),
            true, /* sync_commit */
            result_state,
            state_updates_before_last_checkpoint,
            Some(&sharded_state_cache),
        )
        .unwrap();
}

#[test]
fn test_deleted_key_from_state_store() {
    let executor = TestExecutor::new();
    let db = &executor.db;
    let dummy_state_key1 = StateKey::raw(String::from("test_key1").into_bytes());
    let dummy_value1 = 10u64.le_bytes();
    let dummy_state_key2 = StateKey::raw(String::from("test_key2").into_bytes());
    let dummy_value2 = 20u64.le_bytes();
    // Create test transaction, event and transaction output
    let transaction1 = create_test_transaction(0);
    let transaction2 = create_test_transaction(1);
    let write_set1 = WriteSetMut::new(vec![(
        dummy_state_key1.clone(),
        WriteOp::legacy_modification(dummy_value1.clone()),
    )])
    .freeze()
    .unwrap();

    let write_set2 = WriteSetMut::new(vec![(
        dummy_state_key2.clone(),
        WriteOp::legacy_modification(dummy_value2.clone()),
    )])
    .freeze()
    .unwrap();

    apply_transaction_by_writeset(db, vec![
        (transaction1, write_set1),
        (transaction2, write_set2),
    ]);

    let state_value1_from_db = db
        .reader
        .get_state_value_with_proof_by_version(&dummy_state_key1, 3)
        .unwrap()
        .0
        .unwrap();

    let state_value2_from_db = db
        .reader
        .get_state_value_with_proof_by_version(&dummy_state_key2, 3)
        .unwrap()
        .0
        .unwrap();

    // Ensure both the keys have been successfully written in the DB
    assert_eq!(state_value1_from_db, StateValue::from(dummy_value1.clone()));
    assert_eq!(state_value2_from_db, StateValue::from(dummy_value2.clone()));

    let transaction3 = create_test_transaction(2);
    let write_set3 = WriteSetMut::new(vec![(dummy_state_key1.clone(), WriteOp::legacy_deletion())])
        .freeze()
        .unwrap();

    apply_transaction_by_writeset(db, vec![(transaction3, write_set3)]);

    // Ensure the latest version of the value in DB is None (which implies its deleted)
    assert!(db
        .reader
        .get_state_value_with_proof_by_version(&dummy_state_key1, 5)
        .unwrap()
        .0
        .is_none());

    // Ensure the key that was not touched by the transaction is not accidentally deleted
    let state_value_from_db2 = db
        .reader
        .get_state_value_with_proof_by_version(&dummy_state_key2, 5)
        .unwrap()
        .0
        .unwrap();
    assert_eq!(state_value_from_db2, StateValue::from(dummy_value2));

    // Ensure the previous version of the deleted key is not accidentally deleted
    let state_value_from_db1 = db
        .reader
        .get_state_value_with_proof_by_version(&dummy_state_key1, 3)
        .unwrap()
        .0
        .unwrap();

    assert_eq!(state_value_from_db1, StateValue::from(dummy_value1));
}

#[test]
fn test_reconfig_suffix_empty_blocks() {
    let TestExecutor {
        _path,
        db: _,
        executor,
    } = TestExecutor::new();
    let block_a = TestBlock::new(10000, 1, gen_block_id(1));
    // add block gas limit to be consistent with block executor that will add state checkpoint txn
    let mut block_b = TestBlock::new(10000, 1, gen_block_id(2));
    let block_c = TestBlock::new(1, 1, gen_block_id(3));
    let block_d = TestBlock::new(1, 1, gen_block_id(4));
    block_b
        .txns
        .push(encode_reconfiguration_transaction().into());
    let parent_block_id = executor.committed_block_id();
    executor
        .execute_block(
            (block_a.id, block_a.txns).into(),
            parent_block_id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();
    let output = executor
        .execute_block(
            (block_b.id, block_b.txns).into(),
            block_a.id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();
    executor
        .execute_block(
            (block_c.id, block_c.txns).into(),
            block_b.id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();
    executor
        .execute_block(
            (block_d.id, block_d.txns).into(),
            block_c.id,
            TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG,
        )
        .unwrap();

    let ledger_info = gen_ledger_info(20002, output.root_hash(), block_d.id, 1);

    executor
        .commit_blocks(
            vec![block_a.id, block_b.id, block_c.id, block_d.id],
            ledger_info,
        )
        .unwrap();
}

struct TestBlock {
    txns: Vec<SignatureVerifiedTransaction>,
    id: HashValue,
}

impl TestBlock {
    fn new(num_user_txns: u64, amount: u32, id: HashValue) -> Self {
        let txns = if num_user_txns == 0 {
            Vec::new()
        } else {
            block(
                (0..num_user_txns)
                    .map(|index| encode_mint_transaction(gen_address(index), u64::from(amount)))
                    .collect(),
            )
        };
        TestBlock { txns, id }
    }

    fn inner_txns(&self) -> Vec<Transaction> {
        self.txns.iter().map(|t| t.clone().into_inner()).collect()
    }
}

// Executes a list of transactions by executing and immediately committing one at a time. Returns
// the root hash after all transactions are committed.
fn run_transactions_naive(
    transactions: Vec<SignatureVerifiedTransaction>,
    block_executor_onchain_config: BlockExecutorConfigFromOnchain,
) -> HashValue {
    let executor = TestExecutor::new();
    let db = &executor.db;
    let mut ledger_view: ExecutedTrees = db.reader.get_latest_executed_trees().unwrap();

    for txn in transactions {
        let out = ChunkOutput::by_transaction_execution::<MockVM>(
            vec![txn].into(),
            ledger_view
                .verified_state_view(
                    StateViewId::Miscellaneous,
                    Arc::clone(&db.reader),
                    Arc::new(AsyncProofFetcher::new(db.reader.clone())),
                )
                .unwrap(),
            block_executor_onchain_config.clone(),
        )
        .unwrap();
        let (executed, _, _) = out.apply_to_ledger(&ledger_view, None).unwrap();
        let next_ledger_view = executed.result_view();
        let ExecutedChunk {
            result_state,
            ledger_info,
            next_epoch_state: _,
            ledger_update_output,
        } = executed;
        let LedgerUpdateOutput {
            statuses_for_input_txns: _,
            to_commit,
            reconfig_events: _,
            transaction_info_hashes: _,
            state_updates_until_last_checkpoint: state_updates_before_last_checkpoint,
            sharded_state_cache,
            transaction_accumulator: _,
        } = ledger_update_output;
        db.writer
            .save_transactions(
                &to_commit,
                ledger_view.txn_accumulator().num_leaves(),
                ledger_view.state().base_version,
                ledger_info.as_ref(),
                true, /* sync_commit */
                result_state,
                state_updates_before_last_checkpoint,
                Some(&sharded_state_cache),
            )
            .unwrap();
        ledger_view = next_ledger_view;
    }
    ledger_view.txn_accumulator().root_hash()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    #[cfg_attr(feature = "consensus-only-perf-test", ignore)]
    fn test_reconfiguration_with_retry_transaction_status(
        (num_user_txns, reconfig_txn_index) in (10..100u64).prop_flat_map(|num_user_txns| {
            (
                Just(num_user_txns),
                0..num_user_txns - 1 // avoid state checkpoint right after reconfig
            )
        }).no_shrink()) {
            let executor = TestExecutor::new();

            let block_id = gen_block_id(1);
            let mut block = TestBlock::new(num_user_txns, 10, block_id);
            let num_input_txns = block.txns.len() as LeafCount;
            block.txns[reconfig_txn_index as usize] = encode_reconfiguration_transaction().into();

            let parent_block_id = executor.committed_block_id();
            let output = executor.execute_block(
                (block_id, block.txns.clone()).into(), parent_block_id, TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG
            ).unwrap();

            // assert: txns after the reconfiguration are with status "Retry"
            let retry_iter = output.compute_status_for_input_txns().iter()
            .skip_while(|status| matches!(*status, TransactionStatus::Keep(_)));
            prop_assert_eq!(
                retry_iter.take_while(|status| matches!(*status,TransactionStatus::Retry)).count() as u64,
                num_input_txns - reconfig_txn_index - 1
            );

            // commit
            let ledger_info = gen_ledger_info(reconfig_txn_index + 1 /* version */, output.root_hash(), block_id, 1 /* timestamp */);
            executor.commit_blocks(vec![block_id], ledger_info).unwrap();
            let parent_block_id = executor.committed_block_id();

            // retry txns after reconfiguration
            let retry_block_id = gen_block_id(2);
            let retry_output = executor.execute_block(
                (retry_block_id, block.txns.iter().skip(reconfig_txn_index as usize + 1).cloned().collect::<Vec<SignatureVerifiedTransaction>>()).into(), parent_block_id, TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG
            ).unwrap();
            prop_assert!(retry_output.compute_status_for_input_txns().iter().all(|s| matches!(*s, TransactionStatus::Keep(_))));

            // Second block has StateCheckpoint/BlockPrologue transaction added.
            let ledger_version = num_input_txns + 1;

            // commit
            let ledger_info = gen_ledger_info(ledger_version, retry_output.root_hash(), retry_block_id, 12345 /* timestamp */);
            executor.commit_blocks(vec![retry_block_id], ledger_info).unwrap();

            // get txn_infos from db
            let db = executor.db.reader.clone();
            prop_assert_eq!(db.get_latest_version().unwrap(), ledger_version);
            let txn_list = db.get_transactions(1 /* start version */, ledger_version, ledger_version /* ledger version */, false /* fetch events */).unwrap();
            prop_assert_eq!(&block.inner_txns(), &txn_list.transactions[..num_input_txns as usize]);
            let txn_infos = txn_list.proof.transaction_infos;
            let write_sets = db.get_write_set_iterator(1, ledger_version).unwrap().collect::<Result<_>>().unwrap();
            let event_vecs = db.get_events_iterator(1, ledger_version).unwrap().collect::<Result<_>>().unwrap();

            // replay txns in one batch across epoch boundary,
            // and the replayer should deal with `Retry`s automatically
            let replayer = chunk_executor_tests::TestExecutor::new();
            replayer.executor.replay(txn_list.transactions, txn_infos, write_sets, event_vecs, &VerifyExecutionMode::verify_all()).unwrap();
            replayer.executor.commit().unwrap();
            let replayed_db = replayer.db.reader.clone();
            prop_assert_eq!(
                replayed_db.get_accumulator_root_hash(ledger_version).unwrap(),
                db.get_accumulator_root_hash(ledger_version).unwrap()
            );
        }

    #[test]
    #[cfg_attr(feature = "consensus-only-perf-test", ignore)]
    fn test_executor_restart(a_size in 1..30u64, b_size in 1..30u64, amount in any::<u32>()) {
        let TestExecutor { _path, db, executor } = TestExecutor::new();

        let block_a = TestBlock::new(a_size, amount, gen_block_id(1));
        let block_b = TestBlock::new(b_size, amount, gen_block_id(2));

        let mut parent_block_id;
        let mut root_hash;

        // First execute and commit one block, then destroy executor.
        {
            parent_block_id = executor.committed_block_id();
            let output_a = executor.execute_block(
                (block_a.id, block_a.txns.clone()).into(), parent_block_id, TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG
            ).unwrap();
            root_hash = output_a.root_hash();
            // Add one transaction for the state checkpoint.
            let ledger_info = gen_ledger_info(block_a.txns.len() as u64 + 1, root_hash, block_a.id, 1);
            executor.commit_blocks(vec![block_a.id], ledger_info).unwrap();
            parent_block_id = block_a.id;
            drop(executor);
        }

        // Now we construct a new executor and run one more block.
        {
            let executor = BlockExecutor::<MockVM>::new(db);
            let output_b = executor.execute_block((block_b.id, block_b.txns.clone()).into(), parent_block_id, TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG).unwrap();
            root_hash = output_b.root_hash();
            let ledger_info = gen_ledger_info(
                // add two transactions for the state checkpoints
                (block_a.txns.len() + block_b.txns.len() + 2) as u64,
                root_hash,
                block_b.id,
                2,
            );
            executor.commit_blocks(vec![block_b.id], ledger_info).unwrap();
        };

        let expected_root_hash = run_transactions_naive({
            let mut txns = vec![];
            txns.extend(block_a.txns.iter().cloned());
            txns.push(SignatureVerifiedTransaction::Valid(Transaction::StateCheckpoint(block_a.id)));
            txns.extend(block_b.txns.iter().cloned());
            txns.push(SignatureVerifiedTransaction::Valid(Transaction::StateCheckpoint(block_b.id)));
            txns
        }, TEST_BLOCK_EXECUTOR_ONCHAIN_CONFIG);
        prop_assert_eq!(root_hash, expected_root_hash);
    }
}
