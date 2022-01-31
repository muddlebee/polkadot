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

//! The monitor command.

use crate::{
	prelude::*, rpc_helpers::*, signer::Signer, Error, MonitorConfig, SharedConfig,
	SubmissionStrategy,
};
use codec::Encode;
use frame_support::{StorageHasher, Twox64Concat};
use jsonrpsee::{
	core::{
		client::{Subscription, SubscriptionClientT},
		Error as RpcError,
	},
	rpc_params,
	ws_client::WsClient,
};
use sc_transaction_pool_api::TransactionStatus;
use sp_core::storage::StorageKey;
use sp_runtime::Perbill;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Ensure that now is the signed phase.
async fn ensure_signed_phase<T: EPM::Config, B: BlockT>(
	client: &WsClient,
	at: B::Hash,
) -> Result<(), Error<T>> {
	let key = StorageKey(EPM::CurrentPhase::<T>::hashed_key().to_vec());

	let phase = get_storage::<EPM::Phase<BlockNumber>>(client, rpc_params! {key, at})
		.await
		.map_err::<Error<T>, _>(Into::into)?
		.unwrap_or_default();

	if phase.is_signed() {
		Ok(())
	} else {
		Err(Error::IncorrectPhase)
	}
}

/// Ensure that our current `us` have not submitted anything previously.
async fn ensure_no_previous_solution<
	T: EPM::Config + frame_system::Config<AccountId = AccountId>,
	B: BlockT,
>(
	client: &WsClient,
	at: B::Hash,
	us: &AccountId,
) -> Result<(), Error<T>> {
	use EPM::signed::{SignedSubmissionOf, SubmissionIndicesOf};
	const MODULE_PREFIX: &[u8] = b"ElectionProviderMultiPhase";

	let indices_key = storage_value(MODULE_PREFIX, b"SignedSubmissionIndices");

	let indices: SubmissionIndicesOf<T> = get_storage(client, rpc_params! {indices_key, at})
		.await
		.map_err::<Error<T>, _>(Into::into)?
		.unwrap_or_default();

	// TODO(niklasad1): we could fetch the best previous score here if we want.
	for (_score, id) in indices {
		let key = storage_value_by_key::<Twox64Concat>(
			MODULE_PREFIX,
			b"SignedSubmissionsMap",
			&id.encode(),
		);

		if let Some(submission) =
			get_storage::<SignedSubmissionOf<T>>(client, rpc_params! {key, at})
				.await
				.map_err::<Error<T>, _>(Into::into)?
		{
			log::info!("submission: {:?}", submission);
			if &submission.who == us {
				return Err(Error::AlreadySubmitted)
			}
		}
	}

	Ok(())
}

/// Queries the chain for the best solution and checks whether the computed score
/// is better than best known.
async fn ensure_no_better_solution<T: EPM::Config, B: BlockT>(
	client: &WsClient,
	at: B::Hash,
	score: sp_npos_elections::ElectionScore,
	strategy: SubmissionStrategy,
) -> Result<(), Error<T>> {
	match strategy {
		SubmissionStrategy::AlwaysSubmit => Ok(()),
		SubmissionStrategy::OnlySubmitIfLeading => {
			let key = StorageKey(EPM::QueuedSolution::<T>::hashed_key().to_vec());
			let best_score =
				get_storage::<EPM::ReadySolution<AccountId>>(client, rpc_params! {key, at})
					.await
					.map_err::<Error<T>, _>(Into::into)?
					.map(|s| s.score)
					.unwrap_or_default();
			if sp_npos_elections::is_score_better(score, best_score, Perbill::zero()) {
				Ok(())
			} else {
				Err(Error::AlreadyExistSolutionWithBetterScore)
			}
		},
	}
}

macro_rules! monitor_cmd_for { ($runtime:tt) => { paste::paste! {

	/// The monitor command.
	pub(crate) async fn [<monitor_cmd_ $runtime>](
		client: WsClient,
		shared: SharedConfig,
		config: MonitorConfig,
		signer: Signer,
	) -> Result<(), Error<$crate::[<$runtime _runtime_exports>]::Runtime>> {
		use $crate::[<$runtime _runtime_exports>]::*;
		type StakingMinerError = Error<$crate::[<$runtime _runtime_exports>]::Runtime>;

		let (sub, unsub) = if config.listen == "head" {
			("chain_subscribeNewHeads", "chain_unsubscribeNewHeads")
		} else {
			("chain_subscribeFinalizedHeads", "chain_unsubscribeFinalizedHeads")
		};

		let mut subscription: Subscription<Header> = client.subscribe(&sub, None, &unsub).await?;

		let client = Arc::new(client);
		let (tx, mut rx) = mpsc::unbounded_channel::<StakingMinerError>();

		loop {
			let at = tokio::select! {
				maybe_rp = subscription.next() => {
					match maybe_rp {
						Some(Ok(r)) => r,
						// Custom `jsonrpsee` message; should not occur.
						Some(Err(RpcError::SubscriptionClosed(reason))) => {
							log::debug!("[rpc]: subscription closed by the server: {:?}, starting a new one", reason);
							continue;
						}
						Some(Err(e)) => {
							log::error!("[rpc]: subscription failed to decode Header {:?}, this is bug please file an issue", e);
							return Err(e.into());
						}
						// The subscription was dropped, should only happen if:
						//	- the connection was closed.
						//	- the subscription could not need keep up with the server.
						None => {
							log::warn!("[rpc]: restarting header subscription");
							subscription = client.subscribe(&sub, None, &unsub).await?;
							log::warn!(target: LOG_TARGET, "subscription to {} terminated. Retrying..", sub);
							continue
						}
					}
				},
				maybe_err = rx.recv() => {
					match maybe_err {
						Some(err) => return Err(err),
						None => unreachable!("at least one sender kept in the main loop should always return Some; qed"),
					}
				}
			};

			log::info!(target: LOG_TARGET, "subscribing to {:?} / {:?} at: {}", sub, unsub, at.number());

			// Spawn task and non-recoverable errors are sent back to the main task
			// such as if the connection has been closed.
			tokio::spawn(
				send_and_watch_extrinsic(client.clone(), tx.clone(), at, signer.clone(), shared.clone(), config.clone())
			);
		}

		/// Construct extrinsic at given block and watch it.
		async fn send_and_watch_extrinsic(
			client: Arc<WsClient>,
			tx: mpsc::UnboundedSender<StakingMinerError>,
			at: Header,
			signer: Signer,
			shared: SharedConfig,
			config: MonitorConfig,
		) {

			let hash = at.hash();
			log::trace!(target: LOG_TARGET, "new event at #{:?} ({:?})", at.number, hash);

			// if the runtime version has changed, terminate.
			if let Err(err) = crate::check_versions::<Runtime>(&*client).await {
				let _ = tx.send(err.into());
				return;
			}

			// we prefer doing this check before fetching anything into a remote-ext.
			if ensure_signed_phase::<Runtime, Block>(&*client, hash).await.is_err() {
				log::debug!(target: LOG_TARGET, "phase closed, not interested in this block at all.");
				return;
			}

			if ensure_no_previous_solution::<Runtime, Block>(&*client, hash, &signer.account).await.is_err() {
				log::debug!(target: LOG_TARGET, "We already have a solution in this phase, skipping.");
				return;
			}

			// grab an externalities without staking, just the election snapshot.
			let mut ext = match crate::create_election_ext::<Runtime, Block>(
				shared.uri.clone(),
				Some(hash),
				vec![],
			).await {
				Ok(ext) => ext,
				Err(err) => {
					let _ = tx.send(err);
					return;
				}
			};

			// mine a solution, and run feasibility check on it as well.
			let (raw_solution, witness) = match crate::mine_with::<Runtime>(&config.solver, &mut ext, true) {
				Ok(r) => r,
				Err(err) => {
					let _ = tx.send(err.into());
					return;
				}
			};

			let score = raw_solution.score;
			log::info!(target: LOG_TARGET, "mined solution with {:?}", score);

			let nonce = match crate::get_account_info::<Runtime>(&*client, &signer.account, Some(hash)).await {
				Ok(maybe_account) => {
					let acc = maybe_account.expect(crate::signer::SIGNER_ACCOUNT_WILL_EXIST);
					acc.nonce
				}
				Err(err) => {
					let _ = tx.send(err);
					return;
				}
			};

			let tip = 0 as Balance;
			let period = <Runtime as frame_system::Config>::BlockHashCount::get() / 2;
			let current_block = at.number.saturating_sub(1);
			let era = sp_runtime::generic::Era::mortal(period.into(), current_block.into());

			log::trace!(
				target: LOG_TARGET, "transaction mortality: {:?} -> {:?}",
				era.birth(current_block.into()),
				era.death(current_block.into()),
			);

			let extrinsic = ext.execute_with(|| create_uxt(raw_solution, witness, signer.clone(), nonce, tip, era));
			let bytes = sp_core::Bytes(extrinsic.encode());

			if ensure_no_better_solution::<Runtime, Block>(&*client, hash, score, config.submission_strategy).await.is_err() {
				return;
			}

			let mut tx_subscription: Subscription<
					TransactionStatus<<Block as BlockT>::Hash, <Block as BlockT>::Hash>
			> = match client.subscribe(
				"author_submitAndWatchExtrinsic",
				rpc_params! { bytes },
				"author_unwatchExtrinsic"
			).await {
				Ok(sub) => sub,
				Err(RpcError::RestartNeeded(e)) => {
					let _ = tx.send(RpcError::RestartNeeded(e).into());
					return
				},
				Err(why) => {
					// This usually happens when we've been busy with mining for a few blocks, and
					// now we're receiving the subscriptions of blocks in which we were busy. In
					// these blocks, we still don't have a solution, so we re-compute a new solution
					// and submit it with an outdated `Nonce`, which yields most often `Stale`
					// error. NOTE: to improve this overall, and to be able to introduce an array of
					// other fancy features, we should make this multi-threaded and do the
					// computation outside of this callback.
					log::warn!(
						target: LOG_TARGET,
						"failing to submit a transaction {:?}. continuing...",
						why
					);
					return;
				},
			};

			while let Some(rp) = tx_subscription.next().await {
				let status_update = match rp {
					Ok(r) => r,
					// Custom `jsonrpsee` message; should not occur.
					Err(RpcError::SubscriptionClosed(reason)) => {
						log::warn!(
							"[rpc]: subscription closed by the server: {:?}; continuing...",
							reason
						);
						continue
					},
					Err(e) => {
						log::error!("[rpc]: subscription failed to decode TransactionStatus {:?}, this is a bug please file an issue", e);
						let _ = tx.send(e.into());
						return;
					},
				};

				log::trace!(target: LOG_TARGET, "status update {:?}", status_update);
				match status_update {
					TransactionStatus::Ready |
					TransactionStatus::Broadcast(_) |
					TransactionStatus::Future => continue,
					TransactionStatus::InBlock(hash) => {
						log::info!(target: LOG_TARGET, "included at {:?}", hash);
						let key = StorageKey(
							frame_support::storage::storage_prefix(b"System", b"Events").to_vec(),
						);
						let key2 = key.clone();

						let events = match get_storage::<
							Vec<frame_system::EventRecord<Event, <Block as BlockT>::Hash>>,
						>(&*client, rpc_params! { key, hash })
						.await {
							Ok(rp) => rp.unwrap_or_default(),
							Err(RpcHelperError::JsonRpsee(RpcError::RestartNeeded(e))) => {
								let _ = tx.send(RpcError::RestartNeeded(e).into());
								return;
							}
							// Decoding or other RPC error => just terminate the task.
							Err(e) => {
								log::warn!(target: LOG_TARGET, "get_storage [key: {:?}, hash: {:?}] failed: {:?}",
									key2, hash, e
								);
								return;
							}
						};

						log::info!(target: LOG_TARGET, "events at inclusion {:?}", events);
					},
					TransactionStatus::Retracted(hash) => {
						log::info!(target: LOG_TARGET, "Retracted at {:?}", hash);
					},
					TransactionStatus::Finalized(hash) => {
						log::info!(target: LOG_TARGET, "Finalized at {:?}", hash);
						break
					},
					_ => {
						log::warn!(
							target: LOG_TARGET,
							"Stopping listen due to other status {:?}",
							status_update
						);
						break
					},
				};
			}
		}
	}
}}}

monitor_cmd_for!(polkadot);
monitor_cmd_for!(kusama);
monitor_cmd_for!(westend);

fn storage_prefix(m: &[u8], s: &[u8]) -> Vec<u8> {
	let k1 = sp_core::hashing::twox_128(m);
	let k2 = sp_core::hashing::twox_128(s);
	let mut key = Vec::with_capacity(k1.len() + k2.len());
	key.extend_from_slice(&k1);
	key.extend_from_slice(&k2);
	key
}

/// Get storage value.
fn storage_value(m: &[u8], s: &[u8]) -> StorageKey {
	StorageKey(storage_prefix(m, s))
}

/// Get storage value at given key.
fn storage_value_by_key<H: StorageHasher>(m: &[u8], s: &[u8], encoded_key: &[u8]) -> StorageKey {
	let k1 = storage_prefix(m, s);
	let k2 = H::hash(encoded_key);
	let mut key = Vec::with_capacity(k1.len() + k2.as_ref().len());
	key.extend_from_slice(&k1);
	key.extend_from_slice(k2.as_ref());
	StorageKey(key)
}
