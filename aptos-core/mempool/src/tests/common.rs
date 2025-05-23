// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    core_mempool::{CoreMempool, TimelineState},
    network::{BroadcastPeerPriority, MempoolSyncMsg},
};
use anyhow::{format_err, Result};
use gaptos::aptos_compression::client::CompressionClient;
use gaptos::aptos_config::config::{NodeConfig, MAX_APPLICATION_MESSAGE_SIZE};
use aptos_consensus_types::common::{TransactionInProgress, TransactionSummary};
use gaptos::aptos_crypto::{ed25519::Ed25519PrivateKey, PrivateKey, Uniform};
use gaptos::aptos_types::{
    account_address::AccountAddress,
    chain_id::ChainId,
    mempool_status::MempoolStatusCode,
    transaction::{RawTransaction, SignedTransaction, TransactionPayload},
};
use once_cell::sync::Lazy;
use rand::{rngs::StdRng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) fn setup_mempool() -> (CoreMempool, ConsensusMock) {
    let mut config = NodeConfig::generate_random_config();
    config.mempool.broadcast_buckets = vec![0];
    (CoreMempool::new(&config), ConsensusMock::new())
}

pub(crate) fn setup_mempool_with_broadcast_buckets(
    buckets: Vec<u64>,
) -> (CoreMempool, ConsensusMock) {
    let mut config = NodeConfig::generate_random_config();
    config.mempool.broadcast_buckets = buckets;
    (CoreMempool::new(&config), ConsensusMock::new())
}

static ACCOUNTS: Lazy<Vec<AccountAddress>> = Lazy::new(|| {
    vec![
        AccountAddress::random(),
        AccountAddress::random(),
        AccountAddress::random(),
        AccountAddress::random(),
    ]
});

#[derive(Clone, Serialize, Deserialize)]
pub struct TestTransaction {
    pub(crate) address: usize,
    pub(crate) sequence_number: u64,
    pub(crate) gas_price: u64,
    pub(crate) account_seqno: u64,
}

impl TestTransaction {
    pub(crate) const fn new(address: usize, sequence_number: u64, gas_price: u64) -> Self {
        Self {
            address,
            sequence_number,
            gas_price,
            account_seqno: 0,
        }
    }

    pub(crate) fn make_signed_transaction_with_expiration_time(
        &self,
        exp_timestamp_secs: u64,
    ) -> SignedTransaction {
        self.make_signed_transaction_impl(100, exp_timestamp_secs)
    }

    pub(crate) fn make_signed_transaction_with_max_gas_amount(
        &self,
        max_gas_amount: u64,
    ) -> SignedTransaction {
        self.make_signed_transaction_impl(max_gas_amount, u64::MAX)
    }

    pub(crate) fn make_signed_transaction(&self) -> SignedTransaction {
        self.make_signed_transaction_impl(100, u64::MAX)
    }

    fn make_signed_transaction_impl(
        &self,
        max_gas_amount: u64,
        exp_timestamp_secs: u64,
    ) -> SignedTransaction {
        let raw_txn = RawTransaction::new(
            TestTransaction::get_address(self.address),
            self.sequence_number,
            TransactionPayload::GTxnBytes(vec![]),
            max_gas_amount,
            self.gas_price,
            exp_timestamp_secs,
            ChainId::test(),
        );
        let mut seed: [u8; 32] = [0u8; 32];
        seed[..4].copy_from_slice(&[1, 2, 3, 4]);
        let mut rng: StdRng = StdRng::from_seed(seed);
        let privkey = Ed25519PrivateKey::generate(&mut rng);
        raw_txn
            .sign(&privkey, privkey.public_key())
            .expect("Failed to sign raw transaction.")
            .into_inner()
    }

    pub(crate) fn get_address(address: usize) -> AccountAddress {
        ACCOUNTS[address]
    }
}

pub(crate) fn add_txns_to_mempool(
    pool: &mut CoreMempool,
    txns: Vec<TestTransaction>,
) -> Vec<SignedTransaction> {
    let mut transactions = vec![];
    for transaction in txns {
        let txn = transaction.make_signed_transaction();
        pool.send_user_txn(
            (&txn).into(),
            transaction.account_seqno,
            TimelineState::NotReady,
            false,
            None,
            Some(BroadcastPeerPriority::Primary),
        );
        transactions.push(txn);
    }
    transactions
}

pub(crate) fn txn_bytes_len(transaction: TestTransaction) -> u64 {
    let txn = transaction.make_signed_transaction();
    txn.txn_bytes_len() as u64
}

pub(crate) fn send_user_txn(
    pool: &mut CoreMempool,
    transaction: TestTransaction,
) -> Result<SignedTransaction> {
    let txn = transaction.make_signed_transaction();
    add_signed_txn(pool, txn.clone())?;
    Ok(txn)
}

pub(crate) fn add_signed_txn(pool: &mut CoreMempool, transaction: SignedTransaction) -> Result<()> {
    match pool
        .send_user_txn(
            (&transaction).into(),
            0,
            TimelineState::NotReady,
            false,
            None,
            Some(BroadcastPeerPriority::Primary),
        )
        .code
    {
        MempoolStatusCode::Accepted => Ok(()),
        _ => Err(format_err!("insertion failure")),
    }
}

pub(crate) fn batch_add_signed_txn(
    pool: &mut CoreMempool,
    transactions: Vec<SignedTransaction>,
) -> Result<()> {
    for txn in transactions.into_iter() {
        add_signed_txn(pool, txn)?
    }
    Ok(())
}

// Helper struct that keeps state between `.get_block` calls. Imitates work of Consensus.
pub struct ConsensusMock(BTreeMap<TransactionSummary, TransactionInProgress>);

impl ConsensusMock {
    pub(crate) fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub(crate) fn get_block(
        &mut self,
        mempool: &mut CoreMempool,
        max_txns: u64,
        max_bytes: u64,
    ) -> Vec<SignedTransaction> {
        let block = mempool.get_batch(max_txns, max_bytes, true, self.0.clone());
        block.iter().for_each(|t| {
            let txn_summary =
                TransactionSummary::new(t.sender(), t.sequence_number(), t.committed_hash());
            let txn_info = TransactionInProgress::new(t.gas_unit_price());
            self.0.insert(txn_summary, txn_info);
        });
        block
    }
}

/// Decompresses and deserializes the raw message bytes into a message struct
pub fn decompress_and_deserialize(message_bytes: &Vec<u8>) -> MempoolSyncMsg {
    bcs::from_bytes(
        &gaptos::aptos_compression::decompress(
            message_bytes,
            CompressionClient::Mempool,
            MAX_APPLICATION_MESSAGE_SIZE,
        )
        .unwrap(),
    )
    .unwrap()
}
