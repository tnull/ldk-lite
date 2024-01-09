use crate::hex_utils;
use crate::io::{
	PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE, PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
};
use crate::logger::{log_error, Logger};
use crate::Error;

use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::util::persist::KVStore;
use lightning::util::ser::Writeable;
use lightning::{impl_writeable_tlv_based, impl_writeable_tlv_based_enum};

use std::collections::HashMap;
use std::iter::FromIterator;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

/// Represents a payment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentDetails {
	/// The payment hash, i.e., the hash of the `preimage`.
	pub hash: PaymentHash,
	/// The pre-image used by the payment.
	pub preimage: Option<PaymentPreimage>,
	/// The secret used by the payment.
	pub secret: Option<PaymentSecret>,
	/// The amount transferred.
	pub amount_msat: Option<u64>,
	/// The direction of the payment.
	pub direction: PaymentDirection,
	/// The status of the payment.
	pub status: PaymentStatus,
	/// The maximal amount we allow our counterparty to withhold from us when forwarding the
	/// payment.
	///
	/// This is usually only `Some` for payments received via a JIT-channel, in which case the first
	/// inbound payment will pay for the LSP's channel opening fees.
	///
	/// See [`LdkChannelConfig::accept_underpaying_htlcs`] for more information.
	///
	/// [`LdkChannelConfig::accept_underpaying_htlcs`]: lightning::util::config::ChannelConfig::accept_underpaying_htlcs
	pub maximum_counterparty_skimmed_fee_msat: Option<u64>,
}

impl_writeable_tlv_based!(PaymentDetails, {
	(0, hash, required),
	(1, maximum_counterparty_skimmed_fee_msat, option),
	(2, preimage, required),
	(4, secret, required),
	(6, amount_msat, required),
	(8, direction, required),
	(10, status, required)
});

/// Represents the direction of a payment.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PaymentDirection {
	/// The payment is inbound.
	Inbound,
	/// The payment is outbound.
	Outbound,
}

impl_writeable_tlv_based_enum!(PaymentDirection,
	(0, Inbound) => {},
	(1, Outbound) => {};
);

/// Represents the current status of a payment.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PaymentStatus {
	/// The payment is still pending.
	Pending,
	/// The payment succeeded.
	Succeeded,
	/// The payment failed.
	Failed,
}

impl_writeable_tlv_based_enum!(PaymentStatus,
	(0, Pending) => {},
	(2, Succeeded) => {},
	(4, Failed) => {};
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaymentDetailsUpdate {
	pub hash: PaymentHash,
	pub preimage: Option<Option<PaymentPreimage>>,
	pub secret: Option<Option<PaymentSecret>>,
	pub amount_msat: Option<Option<u64>>,
	pub direction: Option<PaymentDirection>,
	pub status: Option<PaymentStatus>,
	pub maximum_counterparty_skimmed_fee_msat: Option<Option<u64>>,
}

impl PaymentDetailsUpdate {
	pub fn new(hash: PaymentHash) -> Self {
		Self {
			hash,
			preimage: None,
			secret: None,
			amount_msat: None,
			direction: None,
			status: None,
			maximum_counterparty_skimmed_fee_msat: None,
		}
	}
}

pub(crate) struct PaymentStore<K: KVStore + Sync + Send, L: Deref>
where
	L::Target: Logger,
{
	payments: Mutex<HashMap<PaymentHash, PaymentDetails>>,
	kv_store: Arc<K>,
	logger: L,
}

impl<K: KVStore + Sync + Send, L: Deref> PaymentStore<K, L>
where
	L::Target: Logger,
{
	pub(crate) fn new(payments: Vec<PaymentDetails>, kv_store: Arc<K>, logger: L) -> Self {
		let payments = Mutex::new(HashMap::from_iter(
			payments.into_iter().map(|payment| (payment.hash, payment)),
		));
		Self { payments, kv_store, logger }
	}

	pub(crate) fn insert(&self, payment: PaymentDetails) -> Result<bool, Error> {
		let mut locked_payments = self.payments.lock().unwrap();

		let hash = payment.hash.clone();
		let updated = locked_payments.insert(hash.clone(), payment.clone()).is_some();
		self.persist_info(&hash, &payment)?;
		Ok(updated)
	}

	pub(crate) fn remove(&self, hash: &PaymentHash) -> Result<(), Error> {
		let store_key = hex_utils::to_string(&hash.0);
		self.kv_store
			.remove(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key,
				false,
			)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Removing payment data for key {}/{}/{} failed due to: {}",
					PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
					PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
					store_key,
					e
				);
				Error::PersistenceFailed
			})
	}

	pub(crate) fn get(&self, hash: &PaymentHash) -> Option<PaymentDetails> {
		self.payments.lock().unwrap().get(hash).cloned()
	}

	pub(crate) fn update(&self, update: &PaymentDetailsUpdate) -> Result<bool, Error> {
		let mut updated = false;
		let mut locked_payments = self.payments.lock().unwrap();

		if let Some(payment) = locked_payments.get_mut(&update.hash) {
			if let Some(preimage_opt) = update.preimage {
				payment.preimage = preimage_opt;
			}

			if let Some(secret_opt) = update.secret {
				payment.secret = secret_opt;
			}

			if let Some(amount_opt) = update.amount_msat {
				payment.amount_msat = amount_opt;
			}

			if let Some(status) = update.status {
				payment.status = status;
			}

			if let Some(maximum_counterparty_skimmed_fee_msat) =
				update.maximum_counterparty_skimmed_fee_msat
			{
				payment.maximum_counterparty_skimmed_fee_msat =
					maximum_counterparty_skimmed_fee_msat
			}

			self.persist_info(&update.hash, payment)?;
			updated = true;
		}

		Ok(updated)
	}

	pub(crate) fn list_filter<F: FnMut(&&PaymentDetails) -> bool>(
		&self, f: F,
	) -> Vec<PaymentDetails> {
		self.payments
			.lock()
			.unwrap()
			.iter()
			.map(|(_, p)| p)
			.filter(f)
			.cloned()
			.collect::<Vec<PaymentDetails>>()
	}

	fn persist_info(&self, hash: &PaymentHash, payment: &PaymentDetails) -> Result<(), Error> {
		let store_key = hex_utils::to_string(&hash.0);
		let data = payment.encode();
		self.kv_store
			.write(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key,
				&data,
			)
			.map_err(|e| {
				log_error!(
					self.logger,
					"Write for key {}/{}/{} failed due to: {}",
					PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
					PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
					store_key,
					e
				);
				Error::PersistenceFailed
			})?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use lightning::util::test_utils::{TestLogger, TestStore};
	use std::sync::Arc;

	#[test]
	fn payment_info_is_persisted() {
		let store = Arc::new(TestStore::new(false));
		let logger = Arc::new(TestLogger::new());
		let payment_store = PaymentStore::new(Vec::new(), Arc::clone(&store), logger);

		let hash = PaymentHash([42u8; 32]);
		assert!(!payment_store.get(&hash).is_some());

		let store_key = hex_utils::to_string(&hash.0);
		assert!(store
			.read(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key
			)
			.is_err());

		let payment = PaymentDetails {
			hash,
			preimage: None,
			secret: None,
			amount_msat: None,
			direction: PaymentDirection::Inbound,
			status: PaymentStatus::Pending,
			maximum_counterparty_skimmed_fee_msat: None,
		};

		assert_eq!(Ok(false), payment_store.insert(payment.clone()));
		assert!(payment_store.get(&hash).is_some());
		assert!(store
			.read(
				PAYMENT_INFO_PERSISTENCE_PRIMARY_NAMESPACE,
				PAYMENT_INFO_PERSISTENCE_SECONDARY_NAMESPACE,
				&store_key
			)
			.is_ok());

		assert_eq!(Ok(true), payment_store.insert(payment));
		assert!(payment_store.get(&hash).is_some());

		let mut update = PaymentDetailsUpdate::new(hash);
		update.status = Some(PaymentStatus::Succeeded);
		assert_eq!(Ok(true), payment_store.update(&update));
		assert!(payment_store.get(&hash).is_some());

		assert_eq!(PaymentStatus::Succeeded, payment_store.get(&hash).unwrap().status);
	}
}
