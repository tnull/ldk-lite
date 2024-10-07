// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use persist::KVStoreWalletPersister;

use crate::logger::{log_error, log_info, log_trace, Logger};

use crate::config::{BDK_CLIENT_CONCURRENCY, BDK_CLIENT_STOP_GAP, BDK_WALLET_SYNC_TIMEOUT_SECS};
use crate::fee_estimator::{ConfirmationTarget, FeeEstimator};
use crate::Error;

use lightning::chain::chaininterface::BroadcasterInterface;

use lightning::events::bump_transaction::{Utxo, WalletSource};
use lightning::ln::msgs::{DecodeError, UnsignedGossipMessage};
use lightning::ln::script::ShutdownScript;
use lightning::sign::{
	ChangeDestinationSource, EntropySource, InMemorySigner, KeyMaterial, KeysManager, NodeSigner,
	OutputSpender, Recipient, SignerProvider, SpendableOutputDescriptor,
};

use lightning::util::message_signing;
use lightning_invoice::RawBolt11Invoice;

use bdk_chain::ChainPosition;
use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::{KeychainKind, PersistedWallet, SignOptions};

use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::blockdata::locktime::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::key::XOnlyPublicKey;
use bitcoin::psbt::Psbt;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{PublicKey, Scalar, Secp256k1, SecretKey, Signing};
use bitcoin::{
	Amount, ScriptBuf, Transaction, TxOut, Txid, WPubkeyHash, WitnessProgram, WitnessVersion,
};

use esplora_client::AsyncClient as EsploraAsyncClient;

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(crate) mod persist;
pub(crate) mod ser;

enum WalletSyncStatus {
	Completed,
	InProgress { subscribers: tokio::sync::broadcast::Sender<Result<(), Error>> },
}

pub(crate) struct Wallet<B: Deref, E: Deref, L: Deref>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	// A BDK on-chain wallet.
	inner: Mutex<PersistedWallet<KVStoreWalletPersister>>,
	esplora_client: EsploraAsyncClient,
	broadcaster: B,
	fee_estimator: E,
	// A Mutex holding the current sync status.
	sync_status: Mutex<WalletSyncStatus>,
	logger: L,
}

impl<B: Deref, E: Deref, L: Deref> Wallet<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	pub(crate) fn new(
		wallet: bdk_wallet::PersistedWallet<KVStoreWalletPersister>,
		esplora_client: EsploraAsyncClient, broadcaster: B, fee_estimator: E, logger: L,
	) -> Self {
		let inner = Mutex::new(wallet);
		let sync_status = Mutex::new(WalletSyncStatus::Completed);
		Self { inner, esplora_client, broadcaster, fee_estimator, sync_status, logger }
	}

	pub(crate) async fn sync(&self) -> Result<(), Error> {
		if let Some(mut sync_receiver) = self.register_or_subscribe_pending_sync() {
			log_info!(self.logger, "Sync in progress, skipping.");
			return sync_receiver.recv().await.map_err(|e| {
				debug_assert!(false, "Failed to receive wallet sync result: {:?}", e);
				log_error!(self.logger, "Failed to receive wallet sync result: {:?}", e);
				Error::WalletOperationFailed
			})?;
		}

		let res = {
			let full_scan_request = self.inner.lock().unwrap().start_full_scan().build();

			let wallet_sync_timeout_fut = tokio::time::timeout(
				Duration::from_secs(BDK_WALLET_SYNC_TIMEOUT_SECS),
				self.esplora_client.full_scan(
					full_scan_request,
					BDK_CLIENT_STOP_GAP,
					BDK_CLIENT_CONCURRENCY,
				),
			);

			match wallet_sync_timeout_fut.await {
				Ok(res) => match res {
					Ok(update) => match self.inner.lock().unwrap().apply_update(update) {
						Ok(()) => Ok(()),
						Err(e) => {
							log_error!(
								self.logger,
								"Sync failed due to chain connection error: {}",
								e
							);
							Err(Error::WalletOperationFailed)
						},
					},
					Err(e) => match *e {
						esplora_client::Error::Reqwest(he) => {
							log_error!(
								self.logger,
								"Sync failed due to HTTP connection error: {}",
								he
							);
							Err(Error::WalletOperationFailed)
						},
						_ => {
							log_error!(self.logger, "Sync failed due to Esplora error: {}", e);
							Err(Error::WalletOperationFailed)
						},
					},
				},
				Err(e) => {
					log_error!(self.logger, "On-chain wallet sync timed out: {}", e);
					Err(Error::WalletOperationTimeout)
				},
			}
		};

		self.propagate_result_to_subscribers(res);

		res
	}

	pub(crate) fn create_funding_transaction(
		&self, output_script: ScriptBuf, amount: Amount, confirmation_target: ConfirmationTarget,
		locktime: LockTime,
	) -> Result<Transaction, Error> {
		let fee_rate = self.fee_estimator.estimate_fee_rate(confirmation_target);

		let mut locked_wallet = self.inner.lock().unwrap();
		let mut tx_builder = locked_wallet.build_tx();

		tx_builder
			.add_recipient(output_script, amount)
			.fee_rate(fee_rate)
			.nlocktime(locktime)
			.enable_rbf();

		let mut psbt = match tx_builder.finish() {
			Ok(psbt) => {
				log_trace!(self.logger, "Created funding PSBT: {:?}", psbt);
				psbt
			},
			Err(err) => {
				log_error!(self.logger, "Failed to create funding transaction: {}", err);
				return Err(err.into());
			},
		};

		match locked_wallet.sign(&mut psbt, SignOptions::default()) {
			Ok(finalized) => {
				if !finalized {
					return Err(Error::OnchainTxCreationFailed);
				}
			},
			Err(err) => {
				log_error!(self.logger, "Failed to create funding transaction: {}", err);
				return Err(err.into());
			},
		}

		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			e
		})?;

		Ok(tx)
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		let address_info = self.inner.lock().unwrap().reveal_next_address(KeychainKind::External);
		Ok(address_info.address)
	}

	fn get_new_internal_address(&self) -> Result<bitcoin::Address, Error> {
		let address_info = self.inner.lock().unwrap().next_unused_address(KeychainKind::Internal);
		Ok(address_info.address)
	}

	pub(crate) fn get_balances(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<(u64, u64), Error> {
		let balance = self.inner.lock().unwrap().balance();

		let (total, spendable) = (
			balance.total().to_sat(),
			balance.trusted_spendable().to_sat().saturating_sub(total_anchor_channels_reserve_sats),
		);

		Ok((total, spendable))
	}

	pub(crate) fn get_spendable_amount_sats(
		&self, total_anchor_channels_reserve_sats: u64,
	) -> Result<u64, Error> {
		self.get_balances(total_anchor_channels_reserve_sats).map(|(_, s)| s)
	}

	/// Send funds to the given address.
	///
	/// If `amount_msat_or_drain` is `None` the wallet will be drained, i.e., all available funds will be
	/// spent.
	pub(crate) fn send_to_address(
		&self, address: &bitcoin::Address, amount_or_drain: Option<Amount>,
	) -> Result<Txid, Error> {
		let confirmation_target = ConfirmationTarget::OnchainPayment;
		let fee_rate = self.fee_estimator.estimate_fee_rate(confirmation_target);

		let tx = {
			let mut locked_wallet = self.inner.lock().unwrap();
			let mut tx_builder = locked_wallet.build_tx();

			if let Some(amount) = amount_or_drain {
				tx_builder
					.add_recipient(address.script_pubkey(), amount)
					.fee_rate(fee_rate)
					.enable_rbf();
			} else {
				tx_builder
					.drain_wallet()
					.drain_to(address.script_pubkey())
					.fee_rate(fee_rate)
					.enable_rbf();
			}

			let mut psbt = match tx_builder.finish() {
				Ok(psbt) => {
					log_trace!(self.logger, "Created PSBT: {:?}", psbt);
					psbt
				},
				Err(err) => {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				},
			};

			match locked_wallet.sign(&mut psbt, SignOptions::default()) {
				Ok(finalized) => {
					if !finalized {
						return Err(Error::OnchainTxCreationFailed);
					}
				},
				Err(err) => {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				},
			}

			psbt.extract_tx().map_err(|e| {
				log_error!(self.logger, "Failed to extract transaction: {}", e);
				e
			})?
		};

		self.broadcaster.broadcast_transactions(&[&tx]);

		let txid = tx.compute_txid();

		if let Some(amount) = amount_or_drain {
			log_info!(
				self.logger,
				"Created new transaction {} sending {}sats on-chain to address {}",
				txid,
				amount.to_sat(),
				address
			);
		} else {
			log_info!(
				self.logger,
				"Created new transaction {} sending all available on-chain funds to address {}",
				txid,
				address
			);
		}

		Ok(txid)
	}

	fn register_or_subscribe_pending_sync(
		&self,
	) -> Option<tokio::sync::broadcast::Receiver<Result<(), Error>>> {
		let mut sync_status_lock = self.sync_status.lock().unwrap();
		match sync_status_lock.deref_mut() {
			WalletSyncStatus::Completed => {
				// We're first to register for a sync.
				let (tx, _) = tokio::sync::broadcast::channel(1);
				*sync_status_lock = WalletSyncStatus::InProgress { subscribers: tx };
				None
			},
			WalletSyncStatus::InProgress { subscribers } => {
				// A sync is in-progress, we subscribe.
				let rx = subscribers.subscribe();
				Some(rx)
			},
		}
	}

	fn propagate_result_to_subscribers(&self, res: Result<(), Error>) {
		// Send the notification to any other tasks that might be waiting on it by now.
		{
			let mut sync_status_lock = self.sync_status.lock().unwrap();
			match sync_status_lock.deref_mut() {
				WalletSyncStatus::Completed => {
					// No sync in-progress, do nothing.
					return;
				},
				WalletSyncStatus::InProgress { subscribers } => {
					// A sync is in-progress, we notify subscribers.
					if subscribers.receiver_count() > 0 {
						match subscribers.send(res) {
							Ok(_) => (),
							Err(e) => {
								debug_assert!(
									false,
									"Failed to send wallet sync result to subscribers: {:?}",
									e
								);
								log_error!(
									self.logger,
									"Failed to send wallet sync result to subscribers: {:?}",
									e
								);
							},
						}
					}
					*sync_status_lock = WalletSyncStatus::Completed;
				},
			}
		}
	}
}

impl<B: Deref, E: Deref, L: Deref> WalletSource for Wallet<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	fn list_confirmed_utxos(&self) -> Result<Vec<Utxo>, ()> {
		let locked_wallet = self.inner.lock().unwrap();
		let mut utxos = Vec::new();
		let confirmed_txs: Vec<Txid> = locked_wallet
			.transactions()
			.filter(|t| matches!(t.chain_position, ChainPosition::Confirmed(_)))
			.map(|t| t.tx_node.txid)
			.collect();
		let unspent_confirmed_utxos =
			locked_wallet.list_unspent().filter(|u| confirmed_txs.contains(&u.outpoint.txid));

		for u in unspent_confirmed_utxos {
			let script_pubkey = u.txout.script_pubkey;
			match script_pubkey.witness_version() {
				Some(version @ WitnessVersion::V0) => {
					let witness_program = WitnessProgram::new(version, script_pubkey.as_bytes())
						.map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;

					let wpkh = WPubkeyHash::from_slice(&witness_program.program().as_bytes())
						.map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;
					let utxo = Utxo::new_v0_p2wpkh(u.outpoint, u.txout.value, &wpkh);
					utxos.push(utxo);
				},
				Some(version @ WitnessVersion::V1) => {
					let witness_program = WitnessProgram::new(version, script_pubkey.as_bytes())
						.map_err(|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						})?;

					XOnlyPublicKey::from_slice(&witness_program.program().as_bytes()).map_err(
						|e| {
							log_error!(self.logger, "Failed to retrieve script payload: {}", e);
						},
					)?;

					let utxo = Utxo {
						outpoint: u.outpoint,
						output: TxOut {
							value: u.txout.value,
							script_pubkey: ScriptBuf::new_witness_program(&witness_program),
						},
						satisfaction_weight: 1 /* empty script_sig */ * WITNESS_SCALE_FACTOR as u64 +
							1 /* witness items */ + 1 /* schnorr sig len */ + 64, /* schnorr sig */
					};
					utxos.push(utxo);
				},
				Some(version) => {
					log_error!(self.logger, "Unexpected witness version: {}", version,);
				},
				None => {
					log_error!(
						self.logger,
						"Tried to use a non-witness script. This must never happen."
					);
					panic!("Tried to use a non-witness script. This must never happen.");
				},
			}
		}

		Ok(utxos)
	}

	fn get_change_script(&self) -> Result<ScriptBuf, ()> {
		let mut locked_wallet = self.inner.lock().unwrap();
		let address_info = locked_wallet.next_unused_address(KeychainKind::Internal);
		Ok(address_info.address.script_pubkey())
	}

	fn sign_psbt(&self, mut psbt: Psbt) -> Result<Transaction, ()> {
		let locked_wallet = self.inner.lock().unwrap();

		// While BDK populates both `witness_utxo` and `non_witness_utxo` fields, LDK does not. As
		// BDK by default doesn't trust the witness UTXO to account for the Segwit bug, we must
		// disable it here as otherwise we fail to sign.
		let mut sign_options = SignOptions::default();
		sign_options.trust_witness_utxo = true;

		match locked_wallet.sign(&mut psbt, sign_options) {
			Ok(_finalized) => {
				// BDK will fail to finalize for all LDK-provided inputs of the PSBT. Unfortunately
				// we can't check more fine grained if it succeeded for all the other inputs here,
				// so we just ignore the returned `finalized` bool.
			},
			Err(err) => {
				log_error!(self.logger, "Failed to sign transaction: {}", err);
				return Err(());
			},
		}

		let tx = psbt.extract_tx().map_err(|e| {
			log_error!(self.logger, "Failed to extract transaction: {}", e);
			()
		})?;

		Ok(tx)
	}
}

/// Similar to [`KeysManager`], but overrides the destination and shutdown scripts so they are
/// directly spendable by the BDK wallet.
pub(crate) struct WalletKeysManager<B: Deref, E: Deref, L: Deref>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	inner: KeysManager,
	wallet: Arc<Wallet<B, E, L>>,
	logger: L,
}

impl<B: Deref, E: Deref, L: Deref> WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	/// Constructs a `WalletKeysManager` that overrides the destination and shutdown scripts.
	///
	/// See [`KeysManager::new`] for more information on `seed`, `starting_time_secs`, and
	/// `starting_time_nanos`.
	pub fn new(
		seed: &[u8; 32], starting_time_secs: u64, starting_time_nanos: u32,
		wallet: Arc<Wallet<B, E, L>>, logger: L,
	) -> Self {
		let inner = KeysManager::new(seed, starting_time_secs, starting_time_nanos);
		Self { inner, wallet, logger }
	}

	pub fn sign_message(&self, msg: &[u8]) -> String {
		message_signing::sign(msg, &self.inner.get_node_secret_key())
	}

	pub fn get_node_secret_key(&self) -> SecretKey {
		self.inner.get_node_secret_key()
	}

	pub fn verify_signature(&self, msg: &[u8], sig: &str, pkey: &PublicKey) -> bool {
		message_signing::verify(msg, sig, pkey)
	}
}

impl<B: Deref, E: Deref, L: Deref> NodeSigner for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
		self.inner.get_node_id(recipient)
	}

	fn ecdh(
		&self, recipient: Recipient, other_key: &PublicKey, tweak: Option<&Scalar>,
	) -> Result<SharedSecret, ()> {
		self.inner.ecdh(recipient, other_key, tweak)
	}

	fn get_inbound_payment_key_material(&self) -> KeyMaterial {
		self.inner.get_inbound_payment_key_material()
	}

	fn sign_invoice(
		&self, invoice: &RawBolt11Invoice, recipient: Recipient,
	) -> Result<RecoverableSignature, ()> {
		self.inner.sign_invoice(invoice, recipient)
	}

	fn sign_gossip_message(&self, msg: UnsignedGossipMessage<'_>) -> Result<Signature, ()> {
		self.inner.sign_gossip_message(msg)
	}

	fn sign_bolt12_invoice(
		&self, invoice: &lightning::offers::invoice::UnsignedBolt12Invoice,
	) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
		self.inner.sign_bolt12_invoice(invoice)
	}

	fn sign_bolt12_invoice_request(
		&self, invoice_request: &lightning::offers::invoice_request::UnsignedInvoiceRequest,
	) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
		self.inner.sign_bolt12_invoice_request(invoice_request)
	}
}

impl<B: Deref, E: Deref, L: Deref> OutputSpender for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	/// See [`KeysManager::spend_spendable_outputs`] for documentation on this method.
	fn spend_spendable_outputs<C: Signing>(
		&self, descriptors: &[&SpendableOutputDescriptor], outputs: Vec<TxOut>,
		change_destination_script: ScriptBuf, feerate_sat_per_1000_weight: u32,
		locktime: Option<LockTime>, secp_ctx: &Secp256k1<C>,
	) -> Result<Transaction, ()> {
		self.inner.spend_spendable_outputs(
			descriptors,
			outputs,
			change_destination_script,
			feerate_sat_per_1000_weight,
			locktime,
			secp_ctx,
		)
	}
}

impl<B: Deref, E: Deref, L: Deref> EntropySource for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	fn get_secure_random_bytes(&self) -> [u8; 32] {
		self.inner.get_secure_random_bytes()
	}
}

impl<B: Deref, E: Deref, L: Deref> SignerProvider for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	type EcdsaSigner = InMemorySigner;

	fn generate_channel_keys_id(
		&self, inbound: bool, channel_value_satoshis: u64, user_channel_id: u128,
	) -> [u8; 32] {
		self.inner.generate_channel_keys_id(inbound, channel_value_satoshis, user_channel_id)
	}

	fn derive_channel_signer(
		&self, channel_value_satoshis: u64, channel_keys_id: [u8; 32],
	) -> Self::EcdsaSigner {
		self.inner.derive_channel_signer(channel_value_satoshis, channel_keys_id)
	}

	fn read_chan_signer(&self, reader: &[u8]) -> Result<Self::EcdsaSigner, DecodeError> {
		self.inner.read_chan_signer(reader)
	}

	fn get_destination_script(&self, _channel_keys_id: [u8; 32]) -> Result<ScriptBuf, ()> {
		let address = self.wallet.get_new_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
		})?;
		Ok(address.script_pubkey())
	}

	fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
		let address = self.wallet.get_new_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
		})?;

		match address.witness_program() {
			Some(program) => ShutdownScript::new_witness_program(&program).map_err(|e| {
				log_error!(self.logger, "Invalid shutdown script: {:?}", e);
			}),
			_ => {
				log_error!(
					self.logger,
					"Tried to use a non-witness address. This must never happen."
				);
				panic!("Tried to use a non-witness address. This must never happen.");
			},
		}
	}
}

impl<B: Deref, E: Deref, L: Deref> ChangeDestinationSource for WalletKeysManager<B, E, L>
where
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	fn get_change_destination_script(&self) -> Result<ScriptBuf, ()> {
		let address = self.wallet.get_new_internal_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
		})?;
		Ok(address.script_pubkey())
	}
}
