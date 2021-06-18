// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.


use std::collections::HashSet;

use lru::LruCache;
use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::stream::StreamExt;
use futures::stream::FuturesUnordered;


use polkadot_node_network_protocol::PeerId;
use polkadot_node_network_protocol::request_response::IncomingRequest;
use polkadot_node_network_protocol::request_response::request::OutgoingResponse;
use polkadot_node_network_protocol::UnifiedReputationChange as Rep;
use polkadot_node_subsystem_util::Fault;
use polkadot_subsystem::SubsystemSender;
use polkadot_subsystem::messages::AllMessages;
use polkadot_subsystem::messages::DisputeCoordinatorMessage;
use polkadot_node_network_protocol::request_response::v1::DisputeRequest;
use polkadot_node_subsystem_util::runtime::RuntimeInfo;

use crate::LOG_TARGET;

const COST_INVALID_REQUEST: Rep = Rep::CostMajor("Received message could not be decoded");
const COST_INVALID_SIGNATURE: Rep = Rep::Malicious("Signatures were invalid");

/// How many statement imports we want to issue in parallel:
pub const MAX_PARALLEL_IMPORTS: usize = 10;

/// State for handling incoming `DisputeRequest` messages.
struct Receiver<Sender> {
	/// Access to session information:
	runtime: RuntimeInfo,

	/// Subsystem sender for communication with other subsystems.
	sender: Sender,

	/// Channel to retrieve incoming requests from.
	receiver: mpsc::Receiver<sc_network::config::IncomingRequest>,

	/// Senders whose requests are currently being processed.
	processing_senders: HashSet<PeerId>,

	/// We keep record of the last banned peers.
	///
	/// This is needed because once we ban a peer, we will very likely still have pending requests
	/// in the incoming channel - we should not waste time recovering availability for those, as we
	/// already know the peer is malicious.
	banned_peers: LruCache<PeerId, ()>,
}

impl<Sender: SubsystemSender> Receiver<Sender> {
	pub fn new(
		sender: Sender,
		receiver: mpsc::Receiver<sc_network::config::IncomingRequest>
	) -> Self {
		let runtime = RuntimeInfo::new(None);
		Self {
			runtime,
			sender,
			receiver,
			processing_senders: HashSet::new(),
			// Size of MAX_PARALLEL_IMPORTS ensures we are going to immediately get rid of any
			// malicious requests still pending in the incoming queue.
			banned_peers: LruCache::new(MAX_PARALLEL_IMPORTS),
		}
	}

	pub async fn run(mut self) {

		let mut pending_out = FuturesUnordered::new();

		while let Some(raw) = self.receiver.next().await {
			let incoming = IncomingRequest::<DisputeRequest>::try_from_raw(
				raw,
				vec![COST_INVALID_REQUEST]
			);
			let incoming = match incoming {
				Err(err) => {
					tracing::debug!(
						target: LOG_TARGET,
						?err,
						"Decoding incoming request failed."
					);
					continue
				}
				Ok(incoming) => incoming,
			};
			let IncomingRequest {
				peer, payload, pending_response,
			} = incoming;

			// Immediately drop requests from peers that already have requests in flight or have
			// been banned recently (flood protection):
			if self.processing_senders.contains(&peer) || self.banned_peers.contains(&peer) {
				continue
			}

			// Wait for a free slot:
			if pending_out.len() >= MAX_PARALLEL_IMPORTS as usize {
				// Wait for one to finish:
				pending_out.next().await;
			}

			let info_result = self.runtime.get_session_info_by_index(
				&mut self.sender,
				payload.0.candidate_receipt.descriptor.relay_parent,
				payload.0.session_index
			).await;
			let info = match info_result {
				Ok(info) => info,
				Err(Fault::Err(err)) => {
					tracing::debug!(
						target: LOG_TARGET,
						?err,
						"Querying session info failed."
					);
					continue
				}
				Err(Fault::Fatal(fatal)) => {
					tracing::info!(
						target: LOG_TARGET,
						?fatal,
						"Querying session info went terribly wrong."
					);
					return
				}
			};

			let votes_result = payload.0.try_into_signed_votes(&info.session_info);
			let (candidate_receipt, valid_vote, invalid_vote) = match votes_result {
				Err(()) => {
					let result = pending_response.send_outgoing_response(
						OutgoingResponse {
							result: Err(()),
							reputation_changes: vec![COST_INVALID_SIGNATURE],
							sent_feedback: None,
						}
					);
					if let Err(()) = result {
						tracing::debug!(
							target: LOG_TARGET,
							?peer,
							"Changing peer reputation failed."
						);
					}
					tracing::info!(
						target: LOG_TARGET,
						?peer,
						"Peer sent us dispute request with invalid signatures!",
					);
					continue
				}
				Ok(votes) => votes,
			};

			let (pending_confirmation, confirmation_rx) = oneshot::channel();
			let candidate_hash = candidate_receipt.hash();
			self.sender.send_message(
				AllMessages::DisputeCoordinator(
					DisputeCoordinatorMessage::ImportStatements {
						candidate_hash,
						candidate_receipt,
						session: valid_vote.0.session_index(),
						statements: vec![valid_vote, invalid_vote],
						pending_confirmation,
					}
				)
			).await;
			pending_out.push(confirmation_rx);
		}
		tracing::debug!(
			target: LOG_TARGET,
			"Incoming request stream exhausted - shutting down?"
		);
	}
}
