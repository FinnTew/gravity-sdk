// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    counters,
    pending_votes::{PendingVotes, VoteReceptionResult},
    util::time_service::{SendTask, TimeService},
};
use gaptos::aptos_config::config::QcAggregatorType;
use aptos_consensus_types::{
    common::Round, delayed_qc_msg::DelayedQcMsg, sync_info::SyncInfo,
    timeout_2chain::TwoChainTimeoutWithPartialSignatures, vote::Vote,
};
use gaptos::aptos_crypto::HashValue;
use gaptos::aptos_logger as aptos_logger;
use gaptos::aptos_logger::{prelude::*, Schema};
use gaptos::aptos_types::{
    ledger_info::LedgerInfoWithPartialSignatures, validator_verifier::ValidatorVerifier,
};
use futures::future::AbortHandle;
use futures_channel::mpsc::UnboundedSender;
use serde::Serialize;
use std::{fmt, sync::Arc, time::Duration};

/// A reason for starting a new round: introduced for monitoring / debug purposes.
#[derive(Serialize, Debug, PartialEq, Eq)]
pub enum NewRoundReason {
    QCReady,
    Timeout,
}

impl fmt::Display for NewRoundReason {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            NewRoundReason::QCReady => write!(f, "QCReady"),
            NewRoundReason::Timeout => write!(f, "TCReady"),
        }
    }
}

/// NewRoundEvents produced by RoundState are guaranteed to be monotonically increasing.
/// NewRoundEvents are consumed by the rest of the system: they can cause sending new proposals
/// or voting for some proposals that wouldn't have been voted otherwise.
/// The duration is populated for debugging and testing
#[derive(Debug)]
pub struct NewRoundEvent {
    pub round: Round,
    pub reason: NewRoundReason,
    pub timeout: Duration,
    pub prev_round_votes: Vec<(HashValue, LedgerInfoWithPartialSignatures)>,
    pub prev_round_timeout_votes: Option<TwoChainTimeoutWithPartialSignatures>,
}

impl fmt::Display for NewRoundEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "NewRoundEvent: [round: {}, reason: {}, timeout: {:?}]",
            self.round, self.reason, self.timeout,
        )
    }
}

/// Determines the maximum round duration based on the round difference between the current
/// round and the ordered round
pub trait RoundTimeInterval: Send + Sync + 'static {
    /// Use the index of the round after the highest quorum certificate to order a block and
    /// return the duration for this round
    ///
    /// Round indices start at 0 (round index = 0 is the first round after the round that led
    /// to the highest ordered round).  Given that round r is the highest round to order a
    /// block, then round index 0 is round r+1.  Note that for genesis does not follow the
    /// 3-chain rule for commits, so round 1 has round index 0.  For example, if one wants
    /// to calculate the round duration of round 6 and the highest ordered round is 3 (meaning
    /// the highest round to order a block is round 5, then the round index is 0.
    fn get_round_duration(&self, round_index_after_ordered_qc: usize) -> Duration;
}

/// Round durations increase exponentially
/// Basically time interval is base * mul^power
/// Where power=max(rounds_since_qc, max_exponent)
#[derive(Clone)]
pub struct ExponentialTimeInterval {
    // Initial time interval duration after a successful quorum ordering.
    base_ms: u64,
    // By how much we increase interval every time
    exponent_base: f64,
    // Maximum time interval won't exceed base * mul^max_pow.
    // Theoretically, setting it means
    // that we rely on synchrony assumptions when the known max messaging delay is
    // max_interval.  Alternatively, we can consider using max_interval to meet partial synchrony
    // assumptions where while delta is unknown, it is <= max_interval.
    max_exponent: usize,
}

impl ExponentialTimeInterval {
    #[cfg(any(test, feature = "fuzzing"))]
    pub fn fixed(duration: Duration) -> Self {
        Self::new(duration, 1.0, 0)
    }

    pub fn new(base: Duration, exponent_base: f64, max_exponent: usize) -> Self {
        assert!(
            max_exponent < 32,
            "max_exponent for RoundStateTimeInterval should be <32"
        );
        assert!(
            exponent_base.powf(max_exponent as f64).ceil() < f64::from(std::u32::MAX),
            "Maximum interval multiplier should be less then u32::Max"
        );
        ExponentialTimeInterval {
            base_ms: base.as_millis() as u64, // any reasonable ms timeout fits u64 perfectly
            exponent_base,
            max_exponent,
        }
    }
}

impl RoundTimeInterval for ExponentialTimeInterval {
    fn get_round_duration(&self, round_index_after_ordered_qc: usize) -> Duration {
        let pow = round_index_after_ordered_qc.min(self.max_exponent) as u32;
        let base_multiplier = self.exponent_base.powf(f64::from(pow));
        let duration_ms = ((self.base_ms as f64) * base_multiplier).ceil() as u64;
        Duration::from_millis(duration_ms)
    }
}

/// `RoundState` contains information about a specific round and moves forward when
/// receives new certificates.
///
/// A round `r` starts in the following cases:
/// * there is a QuorumCert for round `r-1`,
/// * there is a TimeoutCertificate for round `r-1`.
///
/// Round interval calculation is the responsibility of the RoundStateTimeoutInterval trait. It
/// depends on the delta between the current round and the highest ordered round (the intuition is
/// that we want to exponentially grow the interval the further the current round is from the last
/// ordered round).
///
/// Whenever a new round starts a local timeout is set following the round interval. This local
/// timeout is going to send the timeout events once in interval until the new round starts.
pub struct RoundState {
    // Determines the time interval for a round given the number of non-ordered rounds since
    // last ordering.
    time_interval: Box<dyn RoundTimeInterval>,
    // Highest known ordered round as reported by the caller. The caller might choose not to
    // inform the RoundState about certain ordered rounds (e.g., NIL blocks): in this case the
    // ordered round in RoundState might lag behind the ordered round of a block tree.
    highest_ordered_round: Round,
    // Current round is max{highest_qc, highest_tc} + 1.
    current_round: Round,
    // The deadline for the next local timeout event. It is reset every time a new round start, or
    // a previous deadline expires.
    // Represents as Duration since UNIX_EPOCH.
    current_round_deadline: Duration,
    // Service for timer
    time_service: Arc<dyn TimeService>,
    // To send local timeout events to the subscriber (e.g., SMR)
    timeout_sender: gaptos::aptos_channels::Sender<Round>,
    // Votes received for the current round.
    pending_votes: PendingVotes,
    // Vote sent locally for the current round.
    vote_sent: Option<Vote>,
    // The handle to cancel previous timeout task when moving to next round.
    abort_handle: Option<AbortHandle>,
    // Self sender to send delayed QC aggregation events to the round manager.
    delayed_qc_tx: UnboundedSender<DelayedQcMsg>,
    qc_aggregator_type: QcAggregatorType,
}

#[derive(Default, Schema)]
pub struct RoundStateLogSchema<'a> {
    round: Option<Round>,
    highest_ordered_round: Option<Round>,
    #[schema(display)]
    pending_votes: Option<&'a PendingVotes>,
    #[schema(display)]
    self_vote: Option<&'a Vote>,
}

impl<'a> RoundStateLogSchema<'a> {
    pub fn new(state: &'a RoundState) -> Self {
        Self {
            round: Some(state.current_round),
            highest_ordered_round: Some(state.highest_ordered_round),
            pending_votes: Some(&state.pending_votes),
            self_vote: state.vote_sent.as_ref(),
        }
    }
}

impl RoundState {
    pub fn new(
        time_interval: Box<dyn RoundTimeInterval>,
        time_service: Arc<dyn TimeService>,
        timeout_sender: gaptos::aptos_channels::Sender<Round>,
        delayed_qc_tx: UnboundedSender<DelayedQcMsg>,
        qc_aggregator_type: QcAggregatorType,
    ) -> Self {
        // Our counters are initialized lazily, so they're not going to appear in
        // Prometheus if some conditions never happen. Invoking get() function enforces creation.
        counters::QC_ROUNDS_COUNT.get();
        counters::TIMEOUT_ROUNDS_COUNT.get();
        counters::TIMEOUT_COUNT.get();

        let pending_votes = PendingVotes::new(
            time_service.clone(),
            delayed_qc_tx.clone(),
            qc_aggregator_type.clone(),
        );
        Self {
            time_interval,
            highest_ordered_round: 0,
            current_round: 0,
            // current_round_deadline: time_service.get_current_timestamp(),
            current_round_deadline: Duration::from_secs(u64::MAX),
            time_service,
            timeout_sender,
            pending_votes,
            vote_sent: None,
            abort_handle: None,
            delayed_qc_tx,
            qc_aggregator_type,
        }
    }

    /// Return if already voted for timeout
    pub fn is_vote_timeout(&self) -> bool {
        self.vote_sent.as_ref().map_or(false, |v| v.is_timeout())
    }

    /// Return the current round.
    pub fn current_round(&self) -> Round {
        self.current_round
    }

    /// Returns deadline for current round
    pub fn current_round_deadline(&self) -> Duration {
        self.current_round_deadline
    }

    /// In case the local timeout corresponds to the current round, reset the timeout and
    /// return true. Otherwise ignore and return false.
    pub fn process_local_timeout(&mut self, round: Round) -> bool {
        if round != self.current_round {
            return false;
        }
        warn!(round = round, "Local timeout");
        counters::TIMEOUT_COUNT.inc();
        self.setup_timeout(1);
        true
    }

    /// Notify the RoundState about the potentially new QC, TC, and highest ordered round.
    /// Note that some of these values might not be available by the caller.
    pub fn process_certificates(&mut self, sync_info: SyncInfo) -> Option<NewRoundEvent> {
        if sync_info.highest_ordered_round() > self.highest_ordered_round {
            self.highest_ordered_round = sync_info.highest_ordered_round();
        }
        info!(
            round = self.current_round,
            highest_ordered_round = self.highest_ordered_round,
            "Processing certificates With SyncInfo",
        );
        let new_round = sync_info.highest_round() + 1;
        if new_round > self.current_round {
            let (prev_round_votes, prev_round_timeout_votes) = self.pending_votes.drain_votes();

            // Start a new round.
            self.current_round = new_round;
            self.pending_votes = PendingVotes::new(
                self.time_service.clone(),
                self.delayed_qc_tx.clone(),
                self.qc_aggregator_type.clone(),
            );
            self.vote_sent = None;
            let timeout = self.setup_timeout(1);
            // The new round reason is QCReady in case both QC.round + 1 == new_round, otherwise
            // it's Timeout and TC.round + 1 == new_round.
            let new_round_reason = if sync_info.highest_certified_round() + 1 == new_round {
                NewRoundReason::QCReady
            } else {
                NewRoundReason::Timeout
            };
            let new_round_event = NewRoundEvent {
                round: self.current_round,
                reason: new_round_reason,
                timeout,
                prev_round_votes,
                prev_round_timeout_votes,
            };
            info!(round = new_round, "Starting new round: {}", new_round_event);
            return Some(new_round_event);
        }
        None
    }

    pub fn insert_vote(
        &mut self,
        vote: &Vote,
        verifier: &ValidatorVerifier,
    ) -> VoteReceptionResult {
        if vote.vote_data().proposed().round() == self.current_round {
            self.pending_votes.insert_vote(vote, verifier)
        } else {
            VoteReceptionResult::UnexpectedRound(
                vote.vote_data().proposed().round(),
                self.current_round,
            )
        }
    }

    pub fn record_vote(&mut self, vote: Vote) {
        if vote.vote_data().proposed().round() == self.current_round {
            self.vote_sent = Some(vote);
        }
    }

    pub fn process_delayed_qc_msg(
        &mut self,
        validator_verifier: &ValidatorVerifier,
        msg: DelayedQcMsg,
    ) -> VoteReceptionResult {
        let DelayedQcMsg { vote } = msg;
        self.pending_votes
            .process_delayed_qc(validator_verifier, vote)
    }

    pub fn vote_sent(&self) -> Option<Vote> {
        self.vote_sent.clone()
    }

    /// Setup the timeout task and return the duration of the current timeout
    fn setup_timeout(&mut self, multiplier: u32) -> Duration {
        let timeout_sender = self.timeout_sender.clone();
        let timeout = self.setup_deadline(multiplier * 2);
        trace!(
            "Scheduling timeout of {} ms for round {}",
            timeout.as_millis(),
            self.current_round
        );
        let abort_handle = self
            .time_service
            .run_after(timeout, SendTask::make(timeout_sender, self.current_round));
        if let Some(handle) = self.abort_handle.replace(abort_handle) {
            handle.abort();
        }
        timeout
    }

    /// Setup the current round deadline and return the duration of the current round
    fn setup_deadline(&mut self, multiplier: u32) -> Duration {
        let round_index_after_ordered_round = {
            if self.highest_ordered_round == 0 {
                // Genesis doesn't require the 3-chain rule for commit, hence start the index at
                // the round after genesis.
                self.current_round - 1
            } else if self.current_round < self.highest_ordered_round + 3 {
                0
            } else {
                self.current_round - self.highest_ordered_round - 3
            }
        } as usize;
        let timeout = self
            .time_interval
            .get_round_duration(round_index_after_ordered_round)
            * multiplier;
        let now = self.time_service.get_current_timestamp();
        debug!(
            round = self.current_round,
            "{:?} passed since the previous deadline.",
            now.checked_sub(self.current_round_deadline)
                .map_or("0 ms".to_string(), |v| format!("{:?}", v))
        );
        debug!(
            round = self.current_round,
            "Set round deadline to {:?} from now", timeout
        );
        self.current_round_deadline = now + timeout;
        timeout
    }
}
