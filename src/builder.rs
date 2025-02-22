// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use crate::config::{
	default_user_config, Config, DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS, DEFAULT_ESPLORA_SERVER_URL,
	WALLET_KEYS_SEED_LEN,
};
use crate::connection::ConnectionManager;
use crate::event::EventQueue;
use crate::fee_estimator::OnchainFeeEstimator;
use crate::gossip::GossipSource;
use crate::io;
use crate::io::sqlite_store::SqliteStore;
#[cfg(any(vss, vss_test))]
use crate::io::vss_store::VssStore;
use crate::liquidity::LiquiditySource;
use crate::logger::{log_error, log_info, FilesystemLogger, Logger};
use crate::message_handler::NodeCustomMessageHandler;
use crate::payment::store::PaymentStore;
use crate::peer_store::PeerStore;
use crate::tx_broadcaster::TransactionBroadcaster;
use crate::types::{
	ChainMonitor, ChannelManager, DynStore, GossipSync, Graph, KeysManager, MessageRouter,
	OnionMessenger, PeerManager,
};
use crate::wallet::persist::KVStoreWalletPersister;
use crate::wallet::Wallet;
use crate::{LogLevel, Node};

use lightning::chain::{chainmonitor, BestBlock, Watch};
use lightning::io::Cursor;
use lightning::ln::channelmanager::{self, ChainParameters, ChannelManagerReadArgs};
use lightning::ln::msgs::{RoutingMessageHandler, SocketAddress};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler};
use lightning::routing::gossip::NodeAlias;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{
	ProbabilisticScorer, ProbabilisticScoringDecayParameters, ProbabilisticScoringFeeParameters,
};
use lightning::sign::EntropySource;

use lightning::util::persist::{
	read_channel_monitors, CHANNEL_MANAGER_PERSISTENCE_KEY,
	CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::ReadableArgs;
use lightning::util::sweep::OutputSweeper;

use lightning_persister::fs_store::FilesystemStore;

use lightning_transaction_sync::EsploraSyncClient;

use lightning_liquidity::lsps2::client::LSPS2ClientConfig;
use lightning_liquidity::{LiquidityClientConfig, LiquidityManager};

use bdk_wallet::template::Bip84;
use bdk_wallet::KeychainKind;
use bdk_wallet::Wallet as BdkWallet;

use bip39::Mnemonic;

use bitcoin::secp256k1::PublicKey;
use bitcoin::{BlockHash, Network};

#[cfg(any(vss, vss_test))]
use bitcoin::bip32::ChildNumber;
use std::convert::TryInto;
use std::default::Default;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

#[derive(Debug, Clone)]
enum ChainDataSourceConfig {
	Esplora(String),
}

#[derive(Debug, Clone)]
enum EntropySourceConfig {
	SeedFile(String),
	SeedBytes([u8; WALLET_KEYS_SEED_LEN]),
	Bip39Mnemonic { mnemonic: Mnemonic, passphrase: Option<String> },
}

#[derive(Debug, Clone)]
enum GossipSourceConfig {
	P2PNetwork,
	RapidGossipSync(String),
}

#[derive(Debug, Clone)]
struct LiquiditySourceConfig {
	// LSPS2 service's (address, node_id, token)
	lsps2_service: Option<(SocketAddress, PublicKey, Option<String>)>,
}

impl Default for LiquiditySourceConfig {
	fn default() -> Self {
		Self { lsps2_service: None }
	}
}

/// An error encountered during building a [`Node`].
///
/// [`Node`]: crate::Node
#[derive(Debug, Clone, PartialEq)]
pub enum BuildError {
	/// The given seed bytes are invalid, e.g., have invalid length.
	InvalidSeedBytes,
	/// The given seed file is invalid, e.g., has invalid length, or could not be read.
	InvalidSeedFile,
	/// The current system time is invalid, clocks might have gone backwards.
	InvalidSystemTime,
	/// The a read channel monitor is invalid.
	InvalidChannelMonitor,
	/// The given listening addresses are invalid, e.g. too many were passed.
	InvalidListeningAddresses,
	/// The provided alias is invalid.
	InvalidNodeAlias,
	/// We failed to read data from the [`KVStore`].
	///
	/// [`KVStore`]: lightning::util::persist::KVStore
	ReadFailed,
	/// We failed to write data to the [`KVStore`].
	///
	/// [`KVStore`]: lightning::util::persist::KVStore
	WriteFailed,
	/// We failed to access the given `storage_dir_path`.
	StoragePathAccessFailed,
	/// We failed to setup our [`KVStore`].
	///
	/// [`KVStore`]: lightning::util::persist::KVStore
	KVStoreSetupFailed,
	/// We failed to setup the onchain wallet.
	WalletSetupFailed,
	/// We failed to setup the logger.
	LoggerSetupFailed,
}

impl fmt::Display for BuildError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Self::InvalidSeedBytes => write!(f, "Given seed bytes are invalid."),
			Self::InvalidSeedFile => write!(f, "Given seed file is invalid or could not be read."),
			Self::InvalidSystemTime => {
				write!(f, "System time is invalid. Clocks might have gone back in time.")
			},
			Self::InvalidChannelMonitor => {
				write!(f, "Failed to watch a deserialized ChannelMonitor")
			},
			Self::InvalidListeningAddresses => write!(f, "Given listening addresses are invalid."),
			Self::ReadFailed => write!(f, "Failed to read from store."),
			Self::WriteFailed => write!(f, "Failed to write to store."),
			Self::StoragePathAccessFailed => write!(f, "Failed to access the given storage path."),
			Self::KVStoreSetupFailed => write!(f, "Failed to setup KVStore."),
			Self::WalletSetupFailed => write!(f, "Failed to setup onchain wallet."),
			Self::LoggerSetupFailed => write!(f, "Failed to setup the logger."),
			Self::InvalidNodeAlias => write!(f, "Given node alias is invalid."),
		}
	}
}

impl std::error::Error for BuildError {}

/// A builder for an [`Node`] instance, allowing to set some configuration and module choices from
/// the getgo.
///
/// ### Defaults
/// - Wallet entropy is sourced from a `keys_seed` file located under [`Config::storage_dir_path`]
/// - Chain data is sourced from the Esplora endpoint `https://blockstream.info/api`
/// - Gossip data is sourced via the peer-to-peer network
#[derive(Debug)]
pub struct NodeBuilder {
	config: Config,
	entropy_source_config: Option<EntropySourceConfig>,
	chain_data_source_config: Option<ChainDataSourceConfig>,
	gossip_source_config: Option<GossipSourceConfig>,
	liquidity_source_config: Option<LiquiditySourceConfig>,
}

impl NodeBuilder {
	/// Creates a new builder instance with the default configuration.
	pub fn new() -> Self {
		let config = Config::default();
		Self::from_config(config)
	}

	/// Creates a new builder instance from an [`Config`].
	pub fn from_config(config: Config) -> Self {
		let entropy_source_config = None;
		let chain_data_source_config = None;
		let gossip_source_config = None;
		let liquidity_source_config = None;
		Self {
			config,
			entropy_source_config,
			chain_data_source_config,
			gossip_source_config,
			liquidity_source_config,
		}
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a seed file on disk.
	///
	/// If the given file does not exist a new random seed file will be generated and
	/// stored at the given location.
	pub fn set_entropy_seed_path(&mut self, seed_path: String) -> &mut Self {
		self.entropy_source_config = Some(EntropySourceConfig::SeedFile(seed_path));
		self
	}

	/// Configures the [`Node`] instance to source its wallet entropy from the given 64 seed bytes.
	pub fn set_entropy_seed_bytes(&mut self, seed_bytes: Vec<u8>) -> Result<&mut Self, BuildError> {
		if seed_bytes.len() != WALLET_KEYS_SEED_LEN {
			return Err(BuildError::InvalidSeedBytes);
		}
		let mut bytes = [0u8; WALLET_KEYS_SEED_LEN];
		bytes.copy_from_slice(&seed_bytes);
		self.entropy_source_config = Some(EntropySourceConfig::SeedBytes(bytes));
		Ok(self)
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a [BIP 39] mnemonic.
	///
	/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
	pub fn set_entropy_bip39_mnemonic(
		&mut self, mnemonic: Mnemonic, passphrase: Option<String>,
	) -> &mut Self {
		self.entropy_source_config =
			Some(EntropySourceConfig::Bip39Mnemonic { mnemonic, passphrase });
		self
	}

	/// Configures the [`Node`] instance to source its chain data from the given Esplora server.
	pub fn set_esplora_server(&mut self, esplora_server_url: String) -> &mut Self {
		self.chain_data_source_config = Some(ChainDataSourceConfig::Esplora(esplora_server_url));
		self
	}

	/// Configures the [`Node`] instance to source its gossip data from the Lightning peer-to-peer
	/// network.
	pub fn set_gossip_source_p2p(&mut self) -> &mut Self {
		self.gossip_source_config = Some(GossipSourceConfig::P2PNetwork);
		self
	}

	/// Configures the [`Node`] instance to source its gossip data from the given RapidGossipSync
	/// server.
	pub fn set_gossip_source_rgs(&mut self, rgs_server_url: String) -> &mut Self {
		self.gossip_source_config = Some(GossipSourceConfig::RapidGossipSync(rgs_server_url));
		self
	}

	/// Configures the [`Node`] instance to source its inbound liquidity from the given
	/// [LSPS2](https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md)
	/// service.
	///
	/// Will mark the LSP as trusted for 0-confirmation channels, see [`Config::trusted_peers_0conf`].
	///
	/// The given `token` will be used by the LSP to authenticate the user.
	pub fn set_liquidity_source_lsps2(
		&mut self, address: SocketAddress, node_id: PublicKey, token: Option<String>,
	) -> &mut Self {
		// Mark the LSP as trusted for 0conf
		self.config.trusted_peers_0conf.push(node_id.clone());

		let liquidity_source_config =
			self.liquidity_source_config.get_or_insert(LiquiditySourceConfig::default());
		liquidity_source_config.lsps2_service = Some((address, node_id, token));
		self
	}

	/// Sets the used storage directory path.
	pub fn set_storage_dir_path(&mut self, storage_dir_path: String) -> &mut Self {
		self.config.storage_dir_path = storage_dir_path;
		self
	}

	/// Sets the log dir path if logs need to live separate from the storage directory path.
	pub fn set_log_dir_path(&mut self, log_dir_path: String) -> &mut Self {
		self.config.log_dir_path = Some(log_dir_path);
		self
	}

	/// Sets the Bitcoin network used.
	pub fn set_network(&mut self, network: Network) -> &mut Self {
		self.config.network = network;
		self
	}

	/// Sets the IP address and TCP port on which [`Node`] will listen for incoming network connections.
	pub fn set_listening_addresses(
		&mut self, listening_addresses: Vec<SocketAddress>,
	) -> Result<&mut Self, BuildError> {
		if listening_addresses.len() > 100 {
			return Err(BuildError::InvalidListeningAddresses);
		}

		self.config.listening_addresses = Some(listening_addresses);
		Ok(self)
	}

	/// Sets the node alias that will be used when broadcasting announcements to the gossip
	/// network.
	///
	/// The provided alias must be a valid UTF-8 string and no longer than 32 bytes in total.
	pub fn set_node_alias(&mut self, node_alias: String) -> Result<&mut Self, BuildError> {
		let node_alias = sanitize_alias(&node_alias)?;

		self.config.node_alias = Some(node_alias);
		Ok(self)
	}

	/// Sets the level at which [`Node`] will log messages.
	pub fn set_log_level(&mut self, level: LogLevel) -> &mut Self {
		self.config.log_level = level;
		self
	}

	/// Builds a [`Node`] instance with a [`SqliteStore`] backend and according to the options
	/// previously configured.
	pub fn build(&self) -> Result<Node, BuildError> {
		let storage_dir_path = self.config.storage_dir_path.clone();
		fs::create_dir_all(storage_dir_path.clone())
			.map_err(|_| BuildError::StoragePathAccessFailed)?;
		let kv_store = Arc::new(
			SqliteStore::new(
				storage_dir_path.into(),
				Some(io::sqlite_store::SQLITE_DB_FILE_NAME.to_string()),
				Some(io::sqlite_store::KV_TABLE_NAME.to_string()),
			)
			.map_err(|_| BuildError::KVStoreSetupFailed)?,
		);
		self.build_with_store(kv_store)
	}

	/// Builds a [`Node`] instance with a [`FilesystemStore`] backend and according to the options
	/// previously configured.
	pub fn build_with_fs_store(&self) -> Result<Node, BuildError> {
		let mut storage_dir_path: PathBuf = self.config.storage_dir_path.clone().into();
		storage_dir_path.push("fs_store");

		fs::create_dir_all(storage_dir_path.clone())
			.map_err(|_| BuildError::StoragePathAccessFailed)?;
		let kv_store = Arc::new(FilesystemStore::new(storage_dir_path));
		self.build_with_store(kv_store)
	}

	/// Builds a [`Node`] instance with a [`VssStore`] backend and according to the options
	/// previously configured.
	#[cfg(any(vss, vss_test))]
	pub fn build_with_vss_store(&self, url: String, store_id: String) -> Result<Node, BuildError> {
		use bitcoin::key::Secp256k1;

		let logger = setup_logger(&self.config)?;

		let seed_bytes = seed_bytes_from_config(
			&self.config,
			self.entropy_source_config.as_ref(),
			Arc::clone(&logger),
		)?;
		let config = Arc::new(self.config.clone());

		let xprv = bitcoin::bip32::Xpriv::new_master(config.network, &seed_bytes).map_err(|e| {
			log_error!(logger, "Failed to derive master secret: {}", e);
			BuildError::InvalidSeedBytes
		})?;

		let vss_xprv = xprv
			.derive_priv(&Secp256k1::new(), &[ChildNumber::Hardened { index: 877 }])
			.map_err(|e| {
				log_error!(logger, "Failed to derive VSS secret: {}", e);
				BuildError::KVStoreSetupFailed
			})?;

		let vss_seed_bytes: [u8; 32] = vss_xprv.private_key.secret_bytes();

		let vss_store = Arc::new(VssStore::new(url, store_id, vss_seed_bytes));
		build_with_store_internal(
			config,
			self.chain_data_source_config.as_ref(),
			self.gossip_source_config.as_ref(),
			self.liquidity_source_config.as_ref(),
			seed_bytes,
			logger,
			vss_store,
		)
	}

	/// Builds a [`Node`] instance according to the options previously configured.
	pub fn build_with_store(&self, kv_store: Arc<DynStore>) -> Result<Node, BuildError> {
		let logger = setup_logger(&self.config)?;
		let seed_bytes = seed_bytes_from_config(
			&self.config,
			self.entropy_source_config.as_ref(),
			Arc::clone(&logger),
		)?;
		let config = Arc::new(self.config.clone());

		build_with_store_internal(
			config,
			self.chain_data_source_config.as_ref(),
			self.gossip_source_config.as_ref(),
			self.liquidity_source_config.as_ref(),
			seed_bytes,
			logger,
			kv_store,
		)
	}
}

/// A builder for an [`Node`] instance, allowing to set some configuration and module choices from
/// the getgo.
///
/// ### Defaults
/// - Wallet entropy is sourced from a `keys_seed` file located under [`Config::storage_dir_path`]
/// - Chain data is sourced from the Esplora endpoint `https://blockstream.info/api`
/// - Gossip data is sourced via the peer-to-peer network
#[derive(Debug)]
#[cfg(feature = "uniffi")]
pub struct ArcedNodeBuilder {
	inner: RwLock<NodeBuilder>,
}

#[cfg(feature = "uniffi")]
impl ArcedNodeBuilder {
	/// Creates a new builder instance with the default configuration.
	pub fn new() -> Self {
		let inner = RwLock::new(NodeBuilder::new());
		Self { inner }
	}

	/// Creates a new builder instance from an [`Config`].
	pub fn from_config(config: Config) -> Self {
		let inner = RwLock::new(NodeBuilder::from_config(config));
		Self { inner }
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a seed file on disk.
	///
	/// If the given file does not exist a new random seed file will be generated and
	/// stored at the given location.
	pub fn set_entropy_seed_path(&self, seed_path: String) {
		self.inner.write().unwrap().set_entropy_seed_path(seed_path);
	}

	/// Configures the [`Node`] instance to source its wallet entropy from the given 64 seed bytes.
	///
	/// **Note:** Panics if the length of the given `seed_bytes` differs from 64.
	pub fn set_entropy_seed_bytes(&self, seed_bytes: Vec<u8>) -> Result<(), BuildError> {
		self.inner.write().unwrap().set_entropy_seed_bytes(seed_bytes).map(|_| ())
	}

	/// Configures the [`Node`] instance to source its wallet entropy from a [BIP 39] mnemonic.
	///
	/// [BIP 39]: https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki
	pub fn set_entropy_bip39_mnemonic(&self, mnemonic: Mnemonic, passphrase: Option<String>) {
		self.inner.write().unwrap().set_entropy_bip39_mnemonic(mnemonic, passphrase);
	}

	/// Configures the [`Node`] instance to source its chain data from the given Esplora server.
	pub fn set_esplora_server(&self, esplora_server_url: String) {
		self.inner.write().unwrap().set_esplora_server(esplora_server_url);
	}

	/// Configures the [`Node`] instance to source its gossip data from the Lightning peer-to-peer
	/// network.
	pub fn set_gossip_source_p2p(&self) {
		self.inner.write().unwrap().set_gossip_source_p2p();
	}

	/// Configures the [`Node`] instance to source its gossip data from the given RapidGossipSync
	/// server.
	pub fn set_gossip_source_rgs(&self, rgs_server_url: String) {
		self.inner.write().unwrap().set_gossip_source_rgs(rgs_server_url);
	}

	/// Configures the [`Node`] instance to source its inbound liquidity from the given
	/// [LSPS2](https://github.com/BitcoinAndLightningLayerSpecs/lsp/blob/main/LSPS2/README.md)
	/// service.
	///
	/// Will mark the LSP as trusted for 0-confirmation channels, see [`Config::trusted_peers_0conf`].
	///
	/// The given `token` will be used by the LSP to authenticate the user.
	pub fn set_liquidity_source_lsps2(
		&self, address: SocketAddress, node_id: PublicKey, token: Option<String>,
	) {
		self.inner.write().unwrap().set_liquidity_source_lsps2(address, node_id, token);
	}

	/// Sets the used storage directory path.
	pub fn set_storage_dir_path(&self, storage_dir_path: String) {
		self.inner.write().unwrap().set_storage_dir_path(storage_dir_path);
	}

	/// Sets the log dir path if logs need to live separate from the storage directory path.
	pub fn set_log_dir_path(&self, log_dir_path: String) {
		self.inner.write().unwrap().set_log_dir_path(log_dir_path);
	}

	/// Sets the Bitcoin network used.
	pub fn set_network(&self, network: Network) {
		self.inner.write().unwrap().set_network(network);
	}

	/// Sets the IP address and TCP port on which [`Node`] will listen for incoming network connections.
	pub fn set_listening_addresses(
		&self, listening_addresses: Vec<SocketAddress>,
	) -> Result<(), BuildError> {
		self.inner.write().unwrap().set_listening_addresses(listening_addresses).map(|_| ())
	}

	/// Sets the node alias that will be used when broadcasting announcements to the gossip
	/// network.
	///
	/// The provided alias must be a valid UTF-8 string and no longer than 32 bytes in total.
	pub fn set_node_alias(&self, node_alias: String) -> Result<(), BuildError> {
		self.inner.write().unwrap().set_node_alias(node_alias).map(|_| ())
	}

	/// Sets the level at which [`Node`] will log messages.
	pub fn set_log_level(&self, level: LogLevel) {
		self.inner.write().unwrap().set_log_level(level);
	}

	/// Builds a [`Node`] instance with a [`SqliteStore`] backend and according to the options
	/// previously configured.
	pub fn build(&self) -> Result<Arc<Node>, BuildError> {
		self.inner.read().unwrap().build().map(Arc::new)
	}

	/// Builds a [`Node`] instance with a [`FilesystemStore`] backend and according to the options
	/// previously configured.
	pub fn build_with_fs_store(&self) -> Result<Arc<Node>, BuildError> {
		self.inner.read().unwrap().build_with_fs_store().map(Arc::new)
	}

	/// Builds a [`Node`] instance according to the options previously configured.
	pub fn build_with_store(&self, kv_store: Arc<DynStore>) -> Result<Arc<Node>, BuildError> {
		self.inner.read().unwrap().build_with_store(kv_store).map(Arc::new)
	}
}

/// Builds a [`Node`] instance according to the options previously configured.
fn build_with_store_internal(
	config: Arc<Config>, chain_data_source_config: Option<&ChainDataSourceConfig>,
	gossip_source_config: Option<&GossipSourceConfig>,
	liquidity_source_config: Option<&LiquiditySourceConfig>, seed_bytes: [u8; 64],
	logger: Arc<FilesystemLogger>, kv_store: Arc<DynStore>,
) -> Result<Node, BuildError> {
	// Initialize the on-chain wallet and chain access
	let xprv = bitcoin::bip32::Xpriv::new_master(config.network, &seed_bytes).map_err(|e| {
		log_error!(logger, "Failed to derive master secret: {}", e);
		BuildError::InvalidSeedBytes
	})?;

	let descriptor = Bip84(xprv, KeychainKind::External);
	let change_descriptor = Bip84(xprv, KeychainKind::Internal);
	let mut wallet_persister =
		KVStoreWalletPersister::new(Arc::clone(&kv_store), Arc::clone(&logger));
	let wallet_opt = BdkWallet::load()
		.descriptor(KeychainKind::External, Some(descriptor.clone()))
		.descriptor(KeychainKind::Internal, Some(change_descriptor.clone()))
		.extract_keys()
		.check_network(config.network)
		.load_wallet(&mut wallet_persister)
		.map_err(|e| {
			log_error!(logger, "Failed to set up wallet: {}", e);
			BuildError::WalletSetupFailed
		})?;
	let bdk_wallet = match wallet_opt {
		Some(wallet) => wallet,
		None => BdkWallet::create(descriptor, change_descriptor)
			.network(config.network)
			.create_wallet(&mut wallet_persister)
			.map_err(|e| {
				log_error!(logger, "Failed to set up wallet: {}", e);
				BuildError::WalletSetupFailed
			})?,
	};

	let (esplora_client, tx_sync, tx_broadcaster, fee_estimator) = match chain_data_source_config {
		Some(ChainDataSourceConfig::Esplora(server_url)) => {
			let mut client_builder = esplora_client::Builder::new(&server_url.clone());
			client_builder = client_builder.timeout(DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS);
			let esplora_client = client_builder.build_async().unwrap();
			let tx_sync = Arc::new(EsploraSyncClient::from_client(
				esplora_client.clone(),
				Arc::clone(&logger),
			));
			let tx_broadcaster = Arc::new(TransactionBroadcaster::new(
				tx_sync.client().clone(),
				Arc::clone(&logger),
			));
			let fee_estimator = Arc::new(OnchainFeeEstimator::new(
				tx_sync.client().clone(),
				Arc::clone(&config),
				Arc::clone(&logger),
			));
			(esplora_client, tx_sync, tx_broadcaster, fee_estimator)
		},
		None => {
			// Default to Esplora client.
			let server_url = DEFAULT_ESPLORA_SERVER_URL.to_string();
			let mut client_builder = esplora_client::Builder::new(&server_url);
			client_builder = client_builder.timeout(DEFAULT_ESPLORA_CLIENT_TIMEOUT_SECS);
			let esplora_client = client_builder.build_async().unwrap();
			let tx_sync = Arc::new(EsploraSyncClient::from_client(
				esplora_client.clone(),
				Arc::clone(&logger),
			));
			let tx_broadcaster = Arc::new(TransactionBroadcaster::new(
				tx_sync.client().clone(),
				Arc::clone(&logger),
			));
			let fee_estimator = Arc::new(OnchainFeeEstimator::new(
				tx_sync.client().clone(),
				Arc::clone(&config),
				Arc::clone(&logger),
			));
			(esplora_client, tx_sync, tx_broadcaster, fee_estimator)
		},
	};

	let runtime = Arc::new(RwLock::new(None));
	let wallet = Arc::new(Wallet::new(
		bdk_wallet,
		wallet_persister,
		esplora_client,
		Arc::clone(&tx_broadcaster),
		Arc::clone(&fee_estimator),
		Arc::clone(&logger),
	));

	// Initialize the ChainMonitor
	let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
		Some(Arc::clone(&tx_sync)),
		Arc::clone(&tx_broadcaster),
		Arc::clone(&logger),
		Arc::clone(&fee_estimator),
		Arc::clone(&kv_store),
	));

	// Initialize the KeysManager
	let cur_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_err(|e| {
		log_error!(logger, "Failed to get current time: {}", e);
		BuildError::InvalidSystemTime
	})?;

	let ldk_seed_bytes: [u8; 32] = xprv.private_key.secret_bytes();
	let keys_manager = Arc::new(KeysManager::new(
		&ldk_seed_bytes,
		cur_time.as_secs(),
		cur_time.subsec_nanos(),
		Arc::clone(&wallet),
		Arc::clone(&logger),
	));

	// Initialize the network graph, scorer, and router
	let network_graph =
		match io::utils::read_network_graph(Arc::clone(&kv_store), Arc::clone(&logger)) {
			Ok(graph) => Arc::new(graph),
			Err(e) => {
				if e.kind() == std::io::ErrorKind::NotFound {
					Arc::new(Graph::new(config.network.into(), Arc::clone(&logger)))
				} else {
					return Err(BuildError::ReadFailed);
				}
			},
		};

	let scorer = match io::utils::read_scorer(
		Arc::clone(&kv_store),
		Arc::clone(&network_graph),
		Arc::clone(&logger),
	) {
		Ok(scorer) => Arc::new(Mutex::new(scorer)),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				let params = ProbabilisticScoringDecayParameters::default();
				Arc::new(Mutex::new(ProbabilisticScorer::new(
					params,
					Arc::clone(&network_graph),
					Arc::clone(&logger),
				)))
			} else {
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let scoring_fee_params = ProbabilisticScoringFeeParameters::default();
	let router = Arc::new(DefaultRouter::new(
		Arc::clone(&network_graph),
		Arc::clone(&logger),
		Arc::clone(&keys_manager),
		Arc::clone(&scorer),
		scoring_fee_params,
	));

	// Read ChannelMonitor state from store
	let mut channel_monitors = match read_channel_monitors(
		Arc::clone(&kv_store),
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
	) {
		Ok(monitors) => monitors,
		Err(e) => {
			if e.kind() == lightning::io::ErrorKind::NotFound {
				Vec::new()
			} else {
				log_error!(logger, "Failed to read channel monitors: {}", e.to_string());
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let mut user_config = default_user_config(&config);
	if liquidity_source_config.and_then(|lsc| lsc.lsps2_service.as_ref()).is_some() {
		// Generally allow claiming underpaying HTLCs as the LSP will skim off some fee. We'll
		// check that they don't take too much before claiming.
		user_config.channel_config.accept_underpaying_htlcs = true;

		// FIXME: When we're an LSPS2 client, set maximum allowed inbound HTLC value in flight
		// to 100%. We should eventually be able to set this on a per-channel basis, but for
		// now we just bump the default for all channels.
		user_config.channel_handshake_config.max_inbound_htlc_value_in_flight_percent_of_channel =
			100;
	}

	// Initialize the ChannelManager
	let channel_manager = {
		if let Ok(res) = kv_store.read(
			CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
			CHANNEL_MANAGER_PERSISTENCE_KEY,
		) {
			let mut reader = Cursor::new(res);
			let channel_monitor_references =
				channel_monitors.iter_mut().map(|(_, chanmon)| chanmon).collect();
			let read_args = ChannelManagerReadArgs::new(
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&fee_estimator),
				Arc::clone(&chain_monitor),
				Arc::clone(&tx_broadcaster),
				Arc::clone(&router),
				Arc::clone(&logger),
				user_config,
				channel_monitor_references,
			);
			let (_hash, channel_manager) =
				<(BlockHash, ChannelManager)>::read(&mut reader, read_args).map_err(|e| {
					log_error!(logger, "Failed to read channel manager from KVStore: {}", e);
					BuildError::ReadFailed
				})?;
			channel_manager
		} else {
			// We're starting a fresh node.
			let genesis_block_hash =
				bitcoin::blockdata::constants::genesis_block(config.network).block_hash();

			let chain_params = ChainParameters {
				network: config.network.into(),
				best_block: BestBlock::new(genesis_block_hash, 0),
			};
			channelmanager::ChannelManager::new(
				Arc::clone(&fee_estimator),
				Arc::clone(&chain_monitor),
				Arc::clone(&tx_broadcaster),
				Arc::clone(&router),
				Arc::clone(&logger),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				Arc::clone(&keys_manager),
				user_config,
				chain_params,
				cur_time.as_secs() as u32,
			)
		}
	};

	let channel_manager = Arc::new(channel_manager);

	// Give ChannelMonitors to ChainMonitor
	for (_blockhash, channel_monitor) in channel_monitors.into_iter() {
		let funding_outpoint = channel_monitor.get_funding_txo().0;
		chain_monitor.watch_channel(funding_outpoint, channel_monitor).map_err(|e| {
			log_error!(logger, "Failed to watch channel monitor: {:?}", e);
			BuildError::InvalidChannelMonitor
		})?;
	}

	let message_router = MessageRouter::new(Arc::clone(&network_graph), Arc::clone(&keys_manager));

	// Initialize the PeerManager
	let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
		Arc::clone(&channel_manager),
		Arc::new(message_router),
		Arc::clone(&channel_manager),
		IgnoringMessageHandler {},
		IgnoringMessageHandler {},
	));
	let ephemeral_bytes: [u8; 32] = keys_manager.get_secure_random_bytes();

	// Initialize the GossipSource
	// Use the configured gossip source, if the user set one, otherwise default to P2PNetwork.
	let gossip_source_config = gossip_source_config.unwrap_or(&GossipSourceConfig::P2PNetwork);

	let gossip_source = match gossip_source_config {
		GossipSourceConfig::P2PNetwork => {
			let p2p_source =
				Arc::new(GossipSource::new_p2p(Arc::clone(&network_graph), Arc::clone(&logger)));

			// Reset the RGS sync timestamp in case we somehow switch gossip sources
			io::utils::write_latest_rgs_sync_timestamp(
				0,
				Arc::clone(&kv_store),
				Arc::clone(&logger),
			)
			.map_err(|e| {
				log_error!(logger, "Failed writing to store: {}", e);
				BuildError::WriteFailed
			})?;
			p2p_source
		},
		GossipSourceConfig::RapidGossipSync(rgs_server) => {
			let latest_sync_timestamp = io::utils::read_latest_rgs_sync_timestamp(
				Arc::clone(&kv_store),
				Arc::clone(&logger),
			)
			.unwrap_or(0);
			Arc::new(GossipSource::new_rgs(
				rgs_server.clone(),
				latest_sync_timestamp,
				Arc::clone(&network_graph),
				Arc::clone(&logger),
			))
		},
	};

	let liquidity_source = liquidity_source_config.as_ref().and_then(|lsc| {
		lsc.lsps2_service.as_ref().map(|(address, node_id, token)| {
			let lsps2_client_config = Some(LSPS2ClientConfig {});
			let liquidity_client_config = Some(LiquidityClientConfig { lsps2_client_config });
			let liquidity_manager = Arc::new(LiquidityManager::new(
				Arc::clone(&keys_manager),
				Arc::clone(&channel_manager),
				Some(Arc::clone(&tx_sync)),
				None,
				None,
				liquidity_client_config,
			));
			Arc::new(LiquiditySource::new_lsps2(
				address.clone(),
				*node_id,
				token.clone(),
				Arc::clone(&channel_manager),
				Arc::clone(&keys_manager),
				liquidity_manager,
				Arc::clone(&config),
				Arc::clone(&logger),
			))
		})
	});

	let custom_message_handler = if let Some(liquidity_source) = liquidity_source.as_ref() {
		Arc::new(NodeCustomMessageHandler::new_liquidity(Arc::clone(&liquidity_source)))
	} else {
		Arc::new(NodeCustomMessageHandler::new_ignoring())
	};

	let msg_handler = match gossip_source.as_gossip_sync() {
		GossipSync::P2P(p2p_gossip_sync) => MessageHandler {
			chan_handler: Arc::clone(&channel_manager),
			route_handler: Arc::clone(&p2p_gossip_sync)
				as Arc<dyn RoutingMessageHandler + Sync + Send>,
			onion_message_handler: Arc::clone(&onion_messenger),
			custom_message_handler,
		},
		GossipSync::Rapid(_) => MessageHandler {
			chan_handler: Arc::clone(&channel_manager),
			route_handler: Arc::new(IgnoringMessageHandler {})
				as Arc<dyn RoutingMessageHandler + Sync + Send>,
			onion_message_handler: Arc::clone(&onion_messenger),
			custom_message_handler,
		},
		GossipSync::None => {
			unreachable!("We must always have a gossip sync!");
		},
	};

	let cur_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_err(|e| {
		log_error!(logger, "Failed to get current time: {}", e);
		BuildError::InvalidSystemTime
	})?;

	let peer_manager = Arc::new(PeerManager::new(
		msg_handler,
		cur_time.as_secs().try_into().map_err(|e| {
			log_error!(logger, "Failed to get current time: {}", e);
			BuildError::InvalidSystemTime
		})?,
		&ephemeral_bytes,
		Arc::clone(&logger),
		Arc::clone(&keys_manager),
	));

	liquidity_source.as_ref().map(|l| l.set_peer_manager(Arc::clone(&peer_manager)));

	let connection_manager =
		Arc::new(ConnectionManager::new(Arc::clone(&peer_manager), Arc::clone(&logger)));

	let output_sweeper = match io::utils::read_output_sweeper(
		Arc::clone(&tx_broadcaster),
		Arc::clone(&fee_estimator),
		Arc::clone(&tx_sync),
		Arc::clone(&keys_manager),
		Arc::clone(&kv_store),
		Arc::clone(&logger),
	) {
		Ok(output_sweeper) => Arc::new(output_sweeper),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(OutputSweeper::new(
					channel_manager.current_best_block(),
					Arc::clone(&tx_broadcaster),
					Arc::clone(&fee_estimator),
					Some(Arc::clone(&tx_sync)),
					Arc::clone(&keys_manager),
					Arc::clone(&keys_manager),
					Arc::clone(&kv_store),
					Arc::clone(&logger),
				))
			} else {
				return Err(BuildError::ReadFailed);
			}
		},
	};

	match io::utils::migrate_deprecated_spendable_outputs(
		Arc::clone(&output_sweeper),
		Arc::clone(&kv_store),
		Arc::clone(&logger),
	) {
		Ok(()) => {
			log_info!(logger, "Successfully migrated OutputSweeper data.");
		},
		Err(e) => {
			log_error!(logger, "Failed to migrate OutputSweeper data: {}", e);
			return Err(BuildError::ReadFailed);
		},
	}

	// Init payment info storage
	let payment_store = match io::utils::read_payments(Arc::clone(&kv_store), Arc::clone(&logger)) {
		Ok(payments) => {
			Arc::new(PaymentStore::new(payments, Arc::clone(&kv_store), Arc::clone(&logger)))
		},
		Err(_) => {
			return Err(BuildError::ReadFailed);
		},
	};

	let event_queue = match io::utils::read_event_queue(Arc::clone(&kv_store), Arc::clone(&logger))
	{
		Ok(event_queue) => Arc::new(event_queue),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(EventQueue::new(Arc::clone(&kv_store), Arc::clone(&logger)))
			} else {
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let peer_store = match io::utils::read_peer_info(Arc::clone(&kv_store), Arc::clone(&logger)) {
		Ok(peer_store) => Arc::new(peer_store),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Arc::new(PeerStore::new(Arc::clone(&kv_store), Arc::clone(&logger)))
			} else {
				return Err(BuildError::ReadFailed);
			}
		},
	};

	let (stop_sender, _) = tokio::sync::watch::channel(());
	let (event_handling_stopped_sender, _) = tokio::sync::watch::channel(());

	let is_listening = Arc::new(AtomicBool::new(false));
	let latest_wallet_sync_timestamp = Arc::new(RwLock::new(None));
	let latest_onchain_wallet_sync_timestamp = Arc::new(RwLock::new(None));
	let latest_fee_rate_cache_update_timestamp = Arc::new(RwLock::new(None));
	let latest_rgs_snapshot_timestamp = Arc::new(RwLock::new(None));
	let latest_node_announcement_broadcast_timestamp = Arc::new(RwLock::new(None));
	let latest_channel_monitor_archival_height = Arc::new(RwLock::new(None));

	Ok(Node {
		runtime,
		stop_sender,
		event_handling_stopped_sender,
		config,
		wallet,
		tx_sync,
		tx_broadcaster,
		fee_estimator,
		event_queue,
		channel_manager,
		chain_monitor,
		output_sweeper,
		peer_manager,
		onion_messenger,
		connection_manager,
		keys_manager,
		network_graph,
		gossip_source,
		liquidity_source,
		kv_store,
		logger,
		_router: router,
		scorer,
		peer_store,
		payment_store,
		is_listening,
		latest_wallet_sync_timestamp,
		latest_onchain_wallet_sync_timestamp,
		latest_fee_rate_cache_update_timestamp,
		latest_rgs_snapshot_timestamp,
		latest_node_announcement_broadcast_timestamp,
		latest_channel_monitor_archival_height,
	})
}

fn setup_logger(config: &Config) -> Result<Arc<FilesystemLogger>, BuildError> {
	let log_dir = match &config.log_dir_path {
		Some(log_dir) => String::from(log_dir),
		None => config.storage_dir_path.clone() + "/logs",
	};

	Ok(Arc::new(
		FilesystemLogger::new(log_dir, config.log_level)
			.map_err(|_| BuildError::LoggerSetupFailed)?,
	))
}

fn seed_bytes_from_config(
	config: &Config, entropy_source_config: Option<&EntropySourceConfig>,
	logger: Arc<FilesystemLogger>,
) -> Result<[u8; 64], BuildError> {
	match entropy_source_config {
		Some(EntropySourceConfig::SeedBytes(bytes)) => Ok(bytes.clone()),
		Some(EntropySourceConfig::SeedFile(seed_path)) => {
			Ok(io::utils::read_or_generate_seed_file(seed_path, Arc::clone(&logger))
				.map_err(|_| BuildError::InvalidSeedFile)?)
		},
		Some(EntropySourceConfig::Bip39Mnemonic { mnemonic, passphrase }) => match passphrase {
			Some(passphrase) => Ok(mnemonic.to_seed(passphrase)),
			None => Ok(mnemonic.to_seed("")),
		},
		None => {
			// Default to read or generate from the default location generate a seed file.
			let seed_path = format!("{}/keys_seed", config.storage_dir_path);
			Ok(io::utils::read_or_generate_seed_file(&seed_path, Arc::clone(&logger))
				.map_err(|_| BuildError::InvalidSeedFile)?)
		},
	}
}

/// Sanitize the user-provided node alias to ensure that it is a valid protocol-specified UTF-8 string.
pub(crate) fn sanitize_alias(alias_str: &str) -> Result<NodeAlias, BuildError> {
	let alias = alias_str.trim();

	// Alias must be 32-bytes long or less.
	if alias.as_bytes().len() > 32 {
		return Err(BuildError::InvalidNodeAlias);
	}

	let mut bytes = [0u8; 32];
	bytes[..alias.as_bytes().len()].copy_from_slice(alias.as_bytes());
	Ok(NodeAlias(bytes))
}

#[cfg(test)]
mod tests {
	use super::{sanitize_alias, BuildError, NodeAlias};

	#[test]
	fn sanitize_empty_node_alias() {
		// Empty node alias
		let alias = "";
		let mut buf = [0u8; 32];
		buf[..alias.as_bytes().len()].copy_from_slice(alias.as_bytes());

		let expected_node_alias = NodeAlias([0; 32]);
		let node_alias = sanitize_alias(alias).unwrap();
		assert_eq!(node_alias, expected_node_alias);
	}

	#[test]
	fn sanitize_alias_with_sandwiched_null() {
		// Alias with emojis
		let alias = "I\u{1F496}LDK-Node!";
		let mut buf = [0u8; 32];
		buf[..alias.as_bytes().len()].copy_from_slice(alias.as_bytes());
		let expected_alias = NodeAlias(buf);

		let user_provided_alias = "I\u{1F496}LDK-Node!\0\u{26A1}";
		let node_alias = sanitize_alias(user_provided_alias).unwrap();

		let node_alias_display = format!("{}", node_alias);

		assert_eq!(alias, &node_alias_display);
		assert_ne!(expected_alias, node_alias);
	}

	#[test]
	fn sanitize_alias_gt_32_bytes() {
		let alias = "This is a string longer than thirty-two bytes!"; // 46 bytes
		let node = sanitize_alias(alias);
		assert_eq!(node.err().unwrap(), BuildError::InvalidNodeAlias);
	}
}
