// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Interface between Consensus and Network layers.

use crate::{
    dag::DAGNetworkMessage,
    pipeline,
    quorum_store::types::{Batch, BatchMsg, BatchRequest, BatchResponse},
    rand::rand_gen::network_messages::RandGenMessage,
};
use aptos_config::network_id::{NetworkId, PeerNetworkId};
use aptos_consensus_types::{
    block_retrieval::{BlockRetrievalRequest, BlockRetrievalResponse},
    epoch_retrieval::EpochRetrievalRequest,
    pipeline::{commit_decision::CommitDecision, commit_vote::CommitVote},
    proof_of_store::{ProofOfStoreMsg, SignedBatchInfoMsg},
    proposal_msg::ProposalMsg,
    sync_info::SyncInfo,
    vote_msg::{OrderVoteMsg, VoteMsg},
};
use aptos_network::{
    application::{error::Error, interface::NetworkClientInterface},
    ProtocolId,
};
use aptos_types::{epoch_change::EpochChangeProof, PeerId};
pub use pipeline::commit_reliable_broadcast::CommitMessage;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Network type for consensus
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum ConsensusMsg {
    /// RPC to get a chain of block of the given length starting from the given block id.
    BlockRetrievalRequest(Box<BlockRetrievalRequest>),
    /// Carries the returned blocks and the retrieval status.
    BlockRetrievalResponse(Box<BlockRetrievalResponse>),
    /// Request to get a EpochChangeProof from current_epoch to target_epoch
    EpochRetrievalRequest(Box<EpochRetrievalRequest>),
    /// ProposalMsg contains the required information for the proposer election protocol to make
    /// its choice (typically depends on round and proposer info).
    ProposalMsg(Box<ProposalMsg>),
    /// This struct describes basic synchronization metadata.
    SyncInfo(Box<SyncInfo>),
    /// A vector of LedgerInfo with contiguous increasing epoch numbers to prove a sequence of
    /// epoch changes from the first LedgerInfo's epoch.
    EpochChangeProof(Box<EpochChangeProof>),
    /// VoteMsg is the struct that is ultimately sent by the voter in response for receiving a
    /// proposal.
    VoteMsg(Box<VoteMsg>),
    /// OrderVoteMsg is the struct that is broadcasted by a validator on receiving quorum certificate
    /// on a block.
    OrderVoteMsg(Box<OrderVoteMsg>),
    /// CommitProposal is the struct that is sent by the validator after execution to propose
    /// on the committed state hash root.
    CommitVoteMsg(Box<CommitVote>),
    /// CommitDecision is the struct that is sent by the validator after collecting no fewer
    /// than 2f + 1 signatures on the commit proposal. This part is not on the critical path, but
    /// it can save slow machines to quickly confirm the execution result.
    CommitDecisionMsg(Box<CommitDecision>),
    /// Quorum Store: Send a Batch of transactions.
    BatchMsg(Box<BatchMsg>),
    /// Quorum Store: Request the payloads of a completed batch.
    BatchRequestMsg(Box<BatchRequest>),
    /// Quorum Store: Response to the batch request.
    BatchResponse(Box<Batch>),
    /// Quorum Store: Send a signed batch digest. This is a vote for the batch and a promise that
    /// the batch of transactions was received and will be persisted until batch expiration.
    SignedBatchInfo(Box<SignedBatchInfoMsg>),
    /// Quorum Store: Broadcast a certified proof of store (a digest that received 2f+1 votes).
    ProofOfStoreMsg(Box<ProofOfStoreMsg>),
    /// DAG protocol message
    DAGMessage(DAGNetworkMessage),
    /// Commit message
    CommitMessage(Box<CommitMessage>),
    /// Randomness generation message
    RandGenMessage(RandGenMessage),
    /// Quorum Store: Response to the batch request.
    BatchResponseV2(Box<BatchResponse>),
}

/// Network type for consensus
impl ConsensusMsg {
    /// ConsensusMsg type in string
    ///
    pub fn name(&self) -> &str {
        match self {
            ConsensusMsg::BlockRetrievalRequest(_) => "BlockRetrievalRequest",
            ConsensusMsg::BlockRetrievalResponse(_) => "BlockRetrievalResponse",
            ConsensusMsg::EpochRetrievalRequest(_) => "EpochRetrievalRequest",
            ConsensusMsg::ProposalMsg(_) => "ProposalMsg",
            ConsensusMsg::SyncInfo(_) => "SyncInfo",
            ConsensusMsg::EpochChangeProof(_) => "EpochChangeProof",
            ConsensusMsg::VoteMsg(_) => "VoteMsg",
            ConsensusMsg::OrderVoteMsg(_) => "OrderVoteMsg",
            ConsensusMsg::CommitVoteMsg(_) => "CommitVoteMsg",
            ConsensusMsg::CommitDecisionMsg(_) => "CommitDecisionMsg",
            ConsensusMsg::BatchMsg(_) => "BatchMsg",
            ConsensusMsg::BatchRequestMsg(_) => "BatchRequestMsg",
            ConsensusMsg::BatchResponse(_) => "BatchResponse",
            ConsensusMsg::SignedBatchInfo(_) => "SignedBatchInfo",
            ConsensusMsg::ProofOfStoreMsg(_) => "ProofOfStoreMsg",
            ConsensusMsg::DAGMessage(_) => "DAGMessage",
            ConsensusMsg::CommitMessage(_) => "CommitMessage",
            ConsensusMsg::RandGenMessage(_) => "RandGenMessage",
            ConsensusMsg::BatchResponseV2(_) => "BatchResponseV2",
        }
    }
}

/// The interface from Consensus to Networking layer.
///
/// This is a thin wrapper around a `NetworkClient<ConsensusMsg>`, so it is easy
/// to clone and send off to a separate task. For example, the rpc requests
/// return Futures that encapsulate the whole flow, from sending the request to
/// remote, to finally receiving the response and deserializing. It therefore
/// makes the most sense to make the rpc call on a separate async task, which
/// requires the `ConsensusNetworkClient` to be `Clone` and `Send`.
#[derive(Clone)]
pub struct ConsensusNetworkClient<NetworkClient> {
    network_client: NetworkClient,
}

/// Supported protocols in preferred order (from highest priority to lowest).
pub const RPC: &[ProtocolId] = &[
    ProtocolId::ConsensusRpcCompressed,
    ProtocolId::ConsensusRpcBcs,
    ProtocolId::ConsensusRpcJson,
];

/// Supported protocols in preferred order (from highest priority to lowest).
pub const DIRECT_SEND: &[ProtocolId] = &[
    ProtocolId::ConsensusDirectSendCompressed,
    ProtocolId::ConsensusDirectSendBcs,
    ProtocolId::ConsensusDirectSendJson,
];

impl<NetworkClient: NetworkClientInterface<ConsensusMsg>> ConsensusNetworkClient<NetworkClient> {
    /// Returns a new consensus network client
    pub fn new(network_client: NetworkClient) -> Self {
        Self { network_client }
    }

    /// Send a single message to the destination peer
    pub fn send_to(&self, peer: PeerId, message: ConsensusMsg) -> Result<(), Error> {
        let peer_network_id = self.get_peer_network_id_for_peer(peer);
        self.network_client.send_to_peer(message, peer_network_id)
    }

    /// Send a single message to the destination peers
    pub fn send_to_many(
        &self,
        peers: impl Iterator<Item = PeerId>,
        message: ConsensusMsg,
    ) -> Result<(), Error> {
        let peer_network_ids: Vec<PeerNetworkId> = peers
            .map(|peer| self.get_peer_network_id_for_peer(peer))
            .collect();
        self.network_client
            .send_to_peers(message, &peer_network_ids)
    }

    /// Send a RPC to the destination peer
    pub async fn send_rpc(
        &self,
        peer: PeerId,
        message: ConsensusMsg,
        rpc_timeout: Duration,
    ) -> Result<ConsensusMsg, Error> {
        let peer_network_id = self.get_peer_network_id_for_peer(peer);
        self.network_client
            .send_to_peer_rpc(message, rpc_timeout, peer_network_id)
            .await
    }

    // TODO: we shouldn't need to expose this. Migrate the code to handle
    // peer and network ids.
    fn get_peer_network_id_for_peer(&self, peer: PeerId) -> PeerNetworkId {
        PeerNetworkId::new(NetworkId::Validator, peer)
    }
}
