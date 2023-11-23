use crate::logger::{log_error, log_info, log_trace, Logger};

use crate::Error;

use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};

use lightning::ln::msgs::{DecodeError, UnsignedGossipMessage};
use lightning::ln::script::ShutdownScript;
use lightning::sign::{
	EntropySource, InMemorySigner, KeyMaterial, KeysManager, NodeSigner, Recipient, SignerProvider,
	SpendableOutputDescriptor,
};

use lightning::util::message_signing;

use bdk::blockchain::EsploraBlockchain;
use bdk::database::BatchDatabase;
use bdk::wallet::AddressIndex;
use bdk::FeeRate;
use bdk::{SignOptions, SyncOptions};

use bitcoin::bech32::u5;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{PublicKey, Scalar, Secp256k1, Signing};
use bitcoin::{LockTime, PackedLockTime, Script, Transaction, TxOut, Txid};

use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub struct Wallet<D, B: Deref, E: Deref, L: Deref>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	// A BDK blockchain used for wallet sync.
	blockchain: EsploraBlockchain,
	// A BDK on-chain wallet.
	inner: Mutex<bdk::Wallet<D>>,
	// A cache storing the most recently retrieved fee rate estimations.
	broadcaster: B,
	fee_estimator: E,
	sync_lock: (Mutex<()>, Condvar),
	logger: L,
}

impl<D, B: Deref, E: Deref, L: Deref> Wallet<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	pub(crate) fn new(
		blockchain: EsploraBlockchain, wallet: bdk::Wallet<D>, broadcaster: B, fee_estimator: E,
		logger: L,
	) -> Self {
		let inner = Mutex::new(wallet);
		let sync_lock = (Mutex::new(()), Condvar::new());
		Self { blockchain, inner, broadcaster, fee_estimator, sync_lock, logger }
	}

	pub(crate) async fn sync(&self) -> Result<(), Error> {
		let (lock, cvar) = &self.sync_lock;

		let guard = match lock.try_lock() {
			Ok(guard) => guard,
			Err(_) => {
				log_info!(self.logger, "Sync in progress, skipping.");
				let guard = cvar.wait(lock.lock().unwrap());
				drop(guard);
				cvar.notify_all();
				return Ok(());
			}
		};

		let sync_options = SyncOptions { progress: None };
		let wallet_lock = self.inner.lock().unwrap();
		let res = match wallet_lock.sync(&self.blockchain, sync_options).await {
			Ok(()) => Ok(()),
			Err(e) => match e {
				bdk::Error::Esplora(ref be) => match **be {
					bdk::blockchain::esplora::EsploraError::Reqwest(_) => {
						tokio::time::sleep(Duration::from_secs(1)).await;
						log_error!(
							self.logger,
							"Sync failed due to HTTP connection error, retrying: {}",
							e
						);
						let sync_options = SyncOptions { progress: None };
						wallet_lock
							.sync(&self.blockchain, sync_options)
							.await
							.map_err(|e| From::from(e))
					}
					_ => {
						log_error!(self.logger, "Sync failed due to Esplora error: {}", e);
						Err(From::from(e))
					}
				},
				_ => {
					log_error!(self.logger, "Wallet sync error: {}", e);
					Err(From::from(e))
				}
			},
		};

		drop(guard);
		cvar.notify_all();
		res
	}

	pub(crate) fn create_funding_transaction(
		&self, output_script: Script, value_sats: u64, confirmation_target: ConfirmationTarget,
		locktime: LockTime,
	) -> Result<Transaction, Error> {
		let fee_rate = FeeRate::from_sat_per_kwu(
			self.fee_estimator.get_est_sat_per_1000_weight(confirmation_target) as f32,
		);

		let locked_wallet = self.inner.lock().unwrap();
		let mut tx_builder = locked_wallet.build_tx();

		tx_builder
			.add_recipient(output_script, value_sats)
			.fee_rate(fee_rate)
			.nlocktime(locktime)
			.enable_rbf();

		let mut psbt = match tx_builder.finish() {
			Ok((psbt, _)) => {
				log_trace!(self.logger, "Created funding PSBT: {:?}", psbt);
				psbt
			}
			Err(err) => {
				log_error!(self.logger, "Failed to create funding transaction: {}", err);
				return Err(err.into());
			}
		};

		match locked_wallet.sign(&mut psbt, SignOptions::default()) {
			Ok(finalized) => {
				if !finalized {
					return Err(Error::OnchainTxCreationFailed);
				}
			}
			Err(err) => {
				log_error!(self.logger, "Failed to create funding transaction: {}", err);
				return Err(err.into());
			}
		}

		Ok(psbt.extract_tx())
	}

	pub(crate) fn get_new_address(&self) -> Result<bitcoin::Address, Error> {
		let address_info = self.inner.lock().unwrap().get_address(AddressIndex::New)?;
		Ok(address_info.address)
	}

	pub(crate) fn get_balance(&self) -> Result<bdk::Balance, Error> {
		Ok(self.inner.lock().unwrap().get_balance()?)
	}

	/// Send funds to the given address.
	///
	/// If `amount_msat_or_drain` is `None` the wallet will be drained, i.e., all available funds will be
	/// spent.
	pub(crate) fn send_to_address(
		&self, address: &bitcoin::Address, amount_msat_or_drain: Option<u64>,
	) -> Result<Txid, Error> {
		let confirmation_target = ConfirmationTarget::NonAnchorChannelFee;
		let fee_rate = FeeRate::from_sat_per_kwu(
			self.fee_estimator.get_est_sat_per_1000_weight(confirmation_target) as f32,
		);

		let tx = {
			let locked_wallet = self.inner.lock().unwrap();
			let mut tx_builder = locked_wallet.build_tx();

			if let Some(amount_sats) = amount_msat_or_drain {
				tx_builder
					.add_recipient(address.script_pubkey(), amount_sats)
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
				Ok((psbt, _)) => {
					log_trace!(self.logger, "Created PSBT: {:?}", psbt);
					psbt
				}
				Err(err) => {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				}
			};

			match locked_wallet.sign(&mut psbt, SignOptions::default()) {
				Ok(finalized) => {
					if !finalized {
						return Err(Error::OnchainTxCreationFailed);
					}
				}
				Err(err) => {
					log_error!(self.logger, "Failed to create transaction: {}", err);
					return Err(err.into());
				}
			}
			psbt.extract_tx()
		};

		self.broadcaster.broadcast_transactions(&[&tx]);

		let txid = tx.txid();

		if let Some(amount_sats) = amount_msat_or_drain {
			log_info!(
				self.logger,
				"Created new transaction {} sending {}sats on-chain to address {}",
				txid,
				amount_sats,
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
}

/// Similar to [`KeysManager`], but overrides the destination and shutdown scripts so they are
/// directly spendable by the BDK wallet.
pub struct WalletKeysManager<D, B: Deref, E: Deref, L: Deref>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	inner: KeysManager,
	wallet: Arc<Wallet<D, B, E, L>>,
	logger: L,
}

impl<D, B: Deref, E: Deref, L: Deref> WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
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
		wallet: Arc<Wallet<D, B, E, L>>, logger: L,
	) -> Self {
		let inner = KeysManager::new(seed, starting_time_secs, starting_time_nanos);
		Self { inner, wallet, logger }
	}

	/// See [`KeysManager::spend_spendable_outputs`] for documentation on this method.
	pub fn spend_spendable_outputs<C: Signing>(
		&self, descriptors: &[&SpendableOutputDescriptor], outputs: Vec<TxOut>,
		change_destination_script: Script, feerate_sat_per_1000_weight: u32,
		locktime: Option<PackedLockTime>, secp_ctx: &Secp256k1<C>,
	) -> Result<Option<Transaction>, ()> {
		let only_non_static = &descriptors
			.iter()
			.filter(|desc| !matches!(desc, SpendableOutputDescriptor::StaticOutput { .. }))
			.copied()
			.collect::<Vec<_>>();
		if only_non_static.is_empty() {
			return Ok(None);
		}
		self.inner
			.spend_spendable_outputs(
				only_non_static,
				outputs,
				change_destination_script,
				feerate_sat_per_1000_weight,
				locktime,
				secp_ctx,
			)
			.map(Some)
	}

	pub fn sign_message(&self, msg: &[u8]) -> Result<String, Error> {
		message_signing::sign(msg, &self.inner.get_node_secret_key())
			.or(Err(Error::MessageSigningFailed))
	}

	pub fn verify_signature(&self, msg: &[u8], sig: &str, pkey: &PublicKey) -> bool {
		message_signing::verify(msg, sig, pkey)
	}
}

impl<D, B: Deref, E: Deref, L: Deref> NodeSigner for WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
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
		&self, hrp_bytes: &[u8], invoice_data: &[u5], recipient: Recipient,
	) -> Result<RecoverableSignature, ()> {
		self.inner.sign_invoice(hrp_bytes, invoice_data, recipient)
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

impl<D, B: Deref, E: Deref, L: Deref> EntropySource for WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	fn get_secure_random_bytes(&self) -> [u8; 32] {
		self.inner.get_secure_random_bytes()
	}
}

impl<D, B: Deref, E: Deref, L: Deref> SignerProvider for WalletKeysManager<D, B, E, L>
where
	D: BatchDatabase,
	B::Target: BroadcasterInterface,
	E::Target: FeeEstimator,
	L::Target: Logger,
{
	type Signer = InMemorySigner;

	fn generate_channel_keys_id(
		&self, inbound: bool, channel_value_satoshis: u64, user_channel_id: u128,
	) -> [u8; 32] {
		self.inner.generate_channel_keys_id(inbound, channel_value_satoshis, user_channel_id)
	}

	fn derive_channel_signer(
		&self, channel_value_satoshis: u64, channel_keys_id: [u8; 32],
	) -> Self::Signer {
		self.inner.derive_channel_signer(channel_value_satoshis, channel_keys_id)
	}

	fn read_chan_signer(&self, reader: &[u8]) -> Result<Self::Signer, DecodeError> {
		self.inner.read_chan_signer(reader)
	}

	fn get_destination_script(&self) -> Result<Script, ()> {
		let address = self.wallet.get_new_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
		})?;
		Ok(address.script_pubkey())
	}

	fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
		let address = self.wallet.get_new_address().map_err(|e| {
			log_error!(self.logger, "Failed to retrieve new address from wallet: {}", e);
		})?;

		match address.payload {
			bitcoin::util::address::Payload::WitnessProgram { version, program } => {
				ShutdownScript::new_witness_program(version, &program).map_err(|e| {
					log_error!(self.logger, "Invalid shutdown script: {:?}", e);
				})
			}
			_ => {
				log_error!(
					self.logger,
					"Tried to use a non-witness address. This must never happen."
				);
				panic!("Tried to use a non-witness address. This must never happen.");
			}
		}
	}
}
