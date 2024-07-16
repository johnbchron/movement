use alloy::pubsub::PubSubFrontend;
use alloy::signers::local::PrivateKeySigner;
use alloy_eips::BlockNumberOrTag;
use alloy_network::{Ethereum, EthereumWallet};
use alloy_primitives::private::serde::{Deserialize, Serialize};
use alloy_primitives::{address, Address as EthAddress, FixedBytes, B256, U256};
use alloy_provider::{
	fillers::{ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller, WalletFiller},
	Provider, ProviderBuilder, RootProvider,
};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use alloy_rpc_types::{Filter, Log};
use alloy_sol_types::{sol, SolEvent};
use alloy_transport::BoxTransport;
use alloy_transport_ws::WsConnect;
use anyhow::Context;
use bridge_shared::{
	bridge_contracts::{
		BridgeContractCounterpartyError, BridgeContractInitiator, BridgeContractInitiatorError,
		BridgeContractInitiatorResult,
	},
	bridge_monitoring::{BridgeContractCounterpartyEvent, BridgeContractInitiatorEvent},
	types::{CompletedDetails, LockDetails},
};
use bridge_shared::{
	bridge_monitoring::BridgeContractInitiatorMonitoring,
	types::{
		Amount, BridgeTransferDetails, BridgeTransferId, HashLock, HashLockPreImage,
		InitiatorAddress, RecipientAddress, TimeLock,
	},
};
use futures::{channel::mpsc::UnboundedReceiver, Stream, StreamExt};
use keccak_hash::keccak;
use mcr_settlement_client::send_eth_transaction::{
	send_transaction, InsufficentFunds, SendTransactionErrorRule, UnderPriced, VerifyRule,
};
use std::{fmt::Debug, pin::Pin, task::Poll};
use thiserror::Error;

const INITIATOR_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const COUNTERPARTY_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"; //Dummy val
const RECIPIENT_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const DEFAULT_GAS_LIMIT: u64 = 10_000_000_000;
const MAX_RETRIES: u32 = 5;

type EthHash = [u8; 32];

pub type SCIResult<A, H> = Result<BridgeContractInitiatorEvent<A, H>, BridgeContractInitiatorError>;
pub type SCCResult<H> = Result<BridgeContractCounterpartyEvent<H>, BridgeContractCounterpartyError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveCounterpartyEvent<H> {
	LockedBridgeTransfer(LockDetails<H>),
	CompletedBridgeTransfer(CompletedDetails<H>),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MoveCounterpartyError {
	#[error("Transfer not found")]
	TransferNotFound,
	#[error("Invalid hash lock pre image (secret)")]
	InvalidHashLockPreImage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EthInitiatorEvent<A, H> {
	InitiatedBridgeTransfer(BridgeTransferDetails<A, H>),
	CompletedBridgeTransfer(BridgeTransferId<H>, HashLockPreImage),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EthInitiatorError {
	#[error("Failed to initiate bridge transfer")]
	InitiateTransferError,
	#[error("Transfer not found")]
	TransferNotFound,
	#[error("Invalid hash lock pre image (secret)")]
	InvalidHashLockPreImage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbstractBlockainEvent<A, H> {
	InitiatorContractEvent(SCIResult<A, H>),
	CounterpartyContractEvent(SCCResult<H>),
	Noop,
}

///Configuration for the Ethereum Bridge Client
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
	pub rpc_url: Option<String>,
	pub ws_url: Option<String>,
	pub chain_id: String,
	pub signer_private_key: String,
	pub initiator_address: String,
	pub counterparty_address: String,
	pub recipient_address: String,
	pub gas_limit: u64,
	pub num_tx_send_retries: u32,
}

impl Default for Config {
	fn default() -> Self {
		Config {
			rpc_url: Some("http://localhost:8545".to_string()),
			ws_url: Some("ws://localhost:8545".to_string()),
			chain_id: "31337".to_string(),
			signer_private_key: Self::default_for_private_key(),
			initiator_address: INITIATOR_ADDRESS.to_string(),
			counterparty_address: COUNTERPARTY_ADDRESS.to_string(),
			recipient_address: RECIPIENT_ADDRESS.to_string(),
			gas_limit: DEFAULT_GAS_LIMIT,
			num_tx_send_retries: MAX_RETRIES,
		}
	}
}

impl Config {
	fn default_for_private_key() -> String {
		let random_wallet = PrivateKeySigner::random();
		random_wallet.to_bytes().to_string()
	}
}

// Codegen from the abi
sol!(
	#[allow(missing_docs)]
	#[sol(rpc)]
	AtomicBridgeInitiator,
	"abis/AtomicBridgeInitiator.json"
);

type AlloyProvider = FillProvider<
	JoinFill<
		JoinFill<
			JoinFill<JoinFill<alloy::providers::Identity, GasFiller>, NonceFiller>,
			ChainIdFiller,
		>,
		WalletFiller<EthereumWallet>,
	>,
	RootProvider<BoxTransport>,
	BoxTransport,
	Ethereum,
>;

#[derive(RlpDecodable, RlpEncodable)]
struct EthBridgeTransferDetails {
	pub amount: U256,
	pub originator: EthAddress,
	pub recipient: [u8; 32],
	pub hash_lock: [u8; 32],
	pub time_lock: U256,
	pub state: u8, // Assuming the enum is u8 for now..
}

pub struct EthClient<P> {
	rpc_provider: P,
	chain_id: String,
	ws_provider: RootProvider<PubSubFrontend>,
	initiator_address: EthAddress,
	counterparty_address: EthAddress,
	send_transaction_error_rules: Vec<Box<dyn VerifyRule>>,
	gas_limit: u64,
	num_tx_send_retries: u32,
}

impl EthClient<AlloyProvider> {
	pub async fn build_with_config(
		config: Config,
		counterparty_address: &str,
	) -> Result<Self, anyhow::Error> {
		let signer_private_key = config.signer_private_key;
		let signer = signer_private_key.parse::<PrivateKeySigner>()?;
		println!("Signerrrr: {:?}", signer);
		let initiator_address = config.initiator_address.parse()?;
		println!("Initiator Address: {:?}", initiator_address);
		let rpc_url = config.rpc_url.context("rpc_url not set")?;
		let ws_url = config.ws_url.context("ws_url not set")?;
		let rpc_provider = ProviderBuilder::new()
			.with_recommended_fillers()
			.wallet(EthereumWallet::from(signer.clone()))
			.on_builtin(&rpc_url)
			.await?;
		let ws = WsConnect::new(ws_url);
		let ws_provider = ProviderBuilder::new().on_ws(ws).await?;

		EthClient::build_with_provider(
			rpc_provider,
			ws_provider,
			initiator_address,
			counterparty_address.parse()?,
			counterparty_address.parse()?,
			config.gas_limit,
			config.num_tx_send_retries,
			config.chain_id,
		)
		.await
	}

	async fn build_with_provider(
		rpc_provider: AlloyProvider,
		ws_provider: RootProvider<PubSubFrontend>,
		_signer_address: EthAddress,
		initiator_address: EthAddress,
		counterparty_address: EthAddress,
		gas_limit: u64,
		num_tx_send_retries: u32,
		chain_id: String,
	) -> Result<Self, anyhow::Error> {
		let rule1: Box<dyn VerifyRule> = Box::new(SendTransactionErrorRule::<UnderPriced>::new());
		let rule2: Box<dyn VerifyRule> =
			Box::new(SendTransactionErrorRule::<InsufficentFunds>::new());
		let send_transaction_error_rules = vec![rule1, rule2];
		Ok(EthClient {
			rpc_provider,
			chain_id,
			ws_provider,
			initiator_address,
			counterparty_address,
			send_transaction_error_rules,
			gas_limit,
			num_tx_send_retries,
		})
	}
}

impl<P> Clone for EthClient<P> {
	fn clone(&self) -> Self {
		todo!()
	}
}

#[async_trait::async_trait]
impl<P> BridgeContractInitiator for EthClient<P>
where
	P: Provider + Clone + Send + Sync + Unpin,
{
	type Address = EthAddress;
	type Hash = EthHash;

	async fn initiate_bridge_transfer(
		&mut self,
		_initiator_address: InitiatorAddress<Self::Address>,
		recipient_address: RecipientAddress,
		hash_lock: HashLock<Self::Hash>,
		time_lock: TimeLock,
		amount: Amount,
	) -> BridgeContractInitiatorResult<()> {
		let contract = AtomicBridgeInitiator::new(self.initiator_address, &self.rpc_provider);
		let recipient_bytes: [u8; 32] = recipient_address.0.try_into().unwrap();
		let call = contract.initiateBridgeTransfer(
			U256::from(amount.0),
			FixedBytes(recipient_bytes),
			FixedBytes(hash_lock.0),
			U256::from(time_lock.0),
		);
		let _ = send_transaction(
			call,
			&self.send_transaction_error_rules,
			self.num_tx_send_retries,
			self.gas_limit as u128,
		)
		.await;
		Ok(())
	}

	async fn complete_bridge_transfer(
		&mut self,
		bridge_transfer_id: BridgeTransferId<Self::Hash>,
		pre_image: HashLockPreImage,
	) -> BridgeContractInitiatorResult<()> {
		let pre_image: [u8; 32] =
			vec_to_array(pre_image.0).unwrap_or_else(|_| panic!("Failed to convert pre_image"));
		let contract = AtomicBridgeInitiator::new(self.initiator_address, &self.rpc_provider);
		let call = contract
			.completeBridgeTransfer(FixedBytes(bridge_transfer_id.0), FixedBytes(pre_image));
		let _ = send_transaction(
			call,
			&self.send_transaction_error_rules,
			self.num_tx_send_retries,
			self.gas_limit as u128,
		)
		.await;
		Ok(())
	}

	async fn refund_bridge_transfer(
		&mut self,
		bridge_transfer_id: BridgeTransferId<Self::Hash>,
	) -> BridgeContractInitiatorResult<()> {
		let contract = AtomicBridgeInitiator::new(self.initiator_address, &self.rpc_provider);
		let call = contract.refundBridgeTransfer(FixedBytes(bridge_transfer_id.0));
		let _ = send_transaction(
			call,
			&self.send_transaction_error_rules,
			self.num_tx_send_retries,
			self.gas_limit as u128,
		)
		.await;
		Ok(())
	}

	async fn get_bridge_transfer_details(
		&mut self,
		bridge_transfer_id: BridgeTransferId<Self::Hash>,
	) -> BridgeContractInitiatorResult<Option<BridgeTransferDetails<Self::Address, Self::Hash>>> {
		let mapping_slot = U256::from(0); // the solidity mapping is the zeroth slot in the contract
		let key = bridge_transfer_id.0;
		let storage_slot = self.calculate_storage_slot(key, mapping_slot);
		let storage: U256 = self
			.rpc_provider
			.get_storage_at(self.initiator_address, storage_slot)
			.await
			.unwrap_or_else(|_| panic!("Failed to get storage at slot"));
		let storage_bytes = storage.to_be_bytes::<32>();
		let mut storage_slice = &storage_bytes[..];
		let eth_details = EthBridgeTransferDetails::decode(&mut storage_slice).unwrap();

		let details = BridgeTransferDetails {
			bridge_transfer_id,
			initiator_address: InitiatorAddress(eth_details.originator),
			recipient_address: RecipientAddress(eth_details.recipient.to_vec()),
			hash_lock: HashLock(eth_details.hash_lock),
			time_lock: TimeLock(eth_details.time_lock.wrapping_to::<u64>()),
			amount: Amount(eth_details.amount.wrapping_to::<u64>()),
		};

		Ok(Some(details))
	}
}

impl<P> EthClient<P> {
	fn calculate_storage_slot(&self, key: [u8; 32], mapping_slot: U256) -> U256 {
		#[derive(RlpEncodable)]
		struct SlotKey<'a> {
			key: &'a [u8; 32],
			mapping_slot: U256,
		}

		let slot_key = SlotKey { key: &key, mapping_slot };

		let mut buffer = Vec::new();
		slot_key.encode(&mut buffer);

		let hash = keccak(buffer);
		U256::from_be_slice(&hash.0)
	}
}

pub struct EthInitiatorMonitoring<A, H> {
	listener: UnboundedReceiver<AbstractBlockainEvent<A, H>>,
	ws: RootProvider<PubSubFrontend>,
}

impl<A, H> EthInitiatorMonitoring<A, H>
where
	A: Debug + Default + Send + 'static,
	H: Debug + Default + Send + From<[u8; 32]> + 'static,
{
	async fn run(
		rpc_url: &str,
		listener: UnboundedReceiver<AbstractBlockainEvent<A, H>>,
	) -> Result<Self, anyhow::Error> {
		let ws = WsConnect::new(rpc_url);
		let ws = ProviderBuilder::new().on_ws(ws).await?;

		let initiator_address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
		let filter = Filter::new()
			.address(initiator_address)
			.event("BridgeTransferInitiated(bytes32,address,bytes32,uint256)")
			.event("BridgeTransferCompleted(bytes32,bytes32)")
			.from_block(BlockNumberOrTag::Latest);

		let sub = ws.subscribe_logs(&filter).await?;
		let mut sub_stream = sub.into_stream();

		// Spawn a task to forward events to the listener channel
		let (sender, _) = tokio::sync::mpsc::unbounded_channel::<AbstractBlockainEvent<A, H>>();

		tokio::spawn(async move {
			while let Some(log) = sub_stream.next().await {
				let event =
					AbstractBlockainEvent::InitiatorContractEvent(Ok(convert_log_to_event(log)));
				if sender.send(event).is_err() {
					tracing::error!("Failed to send event to listener channel");
					break;
				}
			}
		});

		Ok(Self { listener, ws })
	}
}

impl<A: Debug, H: Debug> BridgeContractInitiatorMonitoring for EthInitiatorMonitoring<A, H> {
	type Address = A;
	type Hash = H;
}

impl<A: Debug, H: Debug> Stream for EthInitiatorMonitoring<A, H> {
	type Item = BridgeContractInitiatorEvent<
		<Self as BridgeContractInitiatorMonitoring>::Address,
		<Self as BridgeContractInitiatorMonitoring>::Hash,
	>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();
		if let Poll::Ready(Some(AbstractBlockainEvent::InitiatorContractEvent(contract_result))) =
			this.listener.poll_next_unpin(cx)
		{
			tracing::trace!(
				"InitiatorContractMonitoring: Received contract event: {:?}",
				contract_result
			);

			// Only listen to the initiator contract events
			match contract_result {
				Ok(contract_event) => match contract_event {
					BridgeContractInitiatorEvent::Initiated(details) => {
						return Poll::Ready(Some(BridgeContractInitiatorEvent::Initiated(details)));
					}
					BridgeContractInitiatorEvent::Completed(id) => {
						return Poll::Ready(Some(BridgeContractInitiatorEvent::Completed(id)))
					}
					BridgeContractInitiatorEvent::Refunded(id) => {
						return Poll::Ready(Some(BridgeContractInitiatorEvent::Refunded(id)))
					}
				},
				Err(e) => {
					tracing::error!("Error in contract event: {:?}", e);
				}
			}
		}
		Poll::Pending
	}
}

// Utility functions
fn convert_log_to_event<A, H>(log: Log) -> BridgeContractInitiatorEvent<A, H>
where
	A: Default,
	H: Default + From<[u8; 32]>,
{
	let initiated_log = AtomicBridgeInitiator::BridgeTransferInitiated::SIGNATURE_HASH;
	let completed_log = AtomicBridgeInitiator::BridgeTransferCompleted::SIGNATURE_HASH;
	let refunded_log = AtomicBridgeInitiator::BridgeTransferRefunded::SIGNATURE_HASH;

	// Extract details from the log and map to event type
	let topics = log.topics();
	let data = log.data().clone();

	// Assuming the first topic is the event type identifier
	let topic = topics.get(0).expect("Expected event type in topics");

	if topic == &initiated_log {
		// Decode the data for Initiated event
		let tokens = decode_log_data(
			&data,
			&[
				ParamType::FixedBytes(32), // bridge_transfer_id
				ParamType::Address,        // initiator_address
				ParamType::Address,        // recipient_address
				ParamType::FixedBytes(32), // hash_lock
				ParamType::Uint(256),      // time_lock
				ParamType::Uint(256),      // amount
			],
		);

		let bridge_transfer_id =
			BridgeTransferId(H::from(tokens[0].clone().into_fixed_bytes().unwrap()));
		let initiator_address = InitiatorAddress(A::from(
			tokens[1].clone().into_address().unwrap().as_fixed_bytes().try_into().unwrap(),
		));
		let recipient_address = RecipientAddress(tokens[2].clone().into_address().unwrap());
		let hash_lock = HashLock(H::from(tokens[3].clone().into_fixed_bytes().unwrap()));
		let time_lock = TimeLock(tokens[4].clone().into_uint().unwrap().as_u64());
		let amount = Amount(tokens[5].clone().into_uint().unwrap());

		let details = BridgeTransferDetails {
			bridge_transfer_id,
			initiator_address,
			recipient_address,
			hash_lock,
			time_lock,
			amount,
		};

		BridgeContractInitiatorEvent::Initiated(details)
	} else if topic == &completed_log {
		// Decode the data for Completed event
		let bridge_transfer_id = BridgeTransferId(H::from(
			topics
				.get(1)
				.expect("Expected hash in topics")
				.as_fixed_bytes()
				.try_into()
				.unwrap(),
		));
		BridgeContractInitiatorEvent::Completed(bridge_transfer_id)
	} else if topic == &refunded_log {
		// Decode the data for Refunded event
		let bridge_transfer_id = BridgeTransferId(H::from(
			topics
				.get(1)
				.expect("Expected hash in topics")
				.as_fixed_bytes()
				.try_into()
				.unwrap(),
		));
		BridgeContractInitiatorEvent::Refunded(bridge_transfer_id)
	} else {
		unimplemented!("Unexpected event type");
	}
}

fn decode_log_data(name: &str, data: &[u8], params: &[ParamType]) -> Vec<Token> {
	let event = Event {
		name: name.to_string(),
		inputs: params
			.iter()
			.map(|p| ethabi::EventParam { name: "".to_string(), kind: p.clone(), indexed: false })
			.collect(),
		anonymous: false,
	};
	let raw_log = RawLog { topics: vec![], data: data.to_vec() };
	event
		.parse_log(raw_log)
		.expect("Unable to parse log data")
		.params
		.into_iter()
		.map(|p| p.value)
		.collect()
}

fn vec_to_array(vec: Vec<u8>) -> Result<[u8; 32], &'static str> {
	if vec.len() == 32 {
		// Try to convert the Vec<u8> to [u8; 32]
		match vec.try_into() {
			Ok(array) => Ok(array),
			Err(_) => Err("Failed to convert Vec<u8> to [u8; 32]"),
		}
	} else {
		Err("Vec<u8> does not have exactly 32 elements")
	}
}

mod tests {}
