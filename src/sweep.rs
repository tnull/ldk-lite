// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! The output sweeper used to live here before we upstreamed it to `rust-lightning` and migrated
//! to the upstreamed version with LDK Node v0.3.0 (May 2024). We should drop this module entirely
//! once sufficient time has passed for us to be confident any users completed the migration.

use lightning::impl_writeable_tlv_based;
use lightning::ln::types::ChannelId;
use lightning::sign::SpendableOutputDescriptor;

use bitcoin::{Amount, BlockHash, Transaction};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeprecatedSpendableOutputInfo {
	pub(crate) id: [u8; 32],
	pub(crate) descriptor: SpendableOutputDescriptor,
	pub(crate) channel_id: Option<ChannelId>,
	pub(crate) first_broadcast_hash: Option<BlockHash>,
	pub(crate) latest_broadcast_height: Option<u32>,
	pub(crate) latest_spending_tx: Option<Transaction>,
	pub(crate) confirmation_height: Option<u32>,
	pub(crate) confirmation_hash: Option<BlockHash>,
}

impl_writeable_tlv_based!(DeprecatedSpendableOutputInfo, {
	(0, id, required),
	(2, descriptor, required),
	(4, channel_id, option),
	(6, first_broadcast_hash, option),
	(8, latest_broadcast_height, option),
	(10, latest_spending_tx, option),
	(12, confirmation_height, option),
	(14, confirmation_hash, option),
});

pub(crate) fn value_from_descriptor(descriptor: &SpendableOutputDescriptor) -> Amount {
	match &descriptor {
		SpendableOutputDescriptor::StaticOutput { output, .. } => output.value,
		SpendableOutputDescriptor::DelayedPaymentOutput(output) => output.output.value,
		SpendableOutputDescriptor::StaticPaymentOutput(output) => output.output.value,
	}
}
