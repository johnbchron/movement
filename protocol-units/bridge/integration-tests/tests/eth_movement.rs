use alloy::{
	node_bindings::Anvil,
	primitives::{address, keccak256},
	providers::Provider,
};
use bridge_integration_tests::TestHarness;
use bridge_shared::{
	bridge_contracts::BridgeContractInitiator,
	types::{Amount, HashLock, InitiatorAddress, RecipientAddress, TimeLock},
};
use ethereum_bridge::types::EthAddress;
use anyhow::Context;
use aptos_sdk::{
	types::LocalAccount,
	rest_client::{Client, FaucetClient},
	coin_client::CoinClient
}; 
use rand::{rngs::StdRng, SeedableRng}; 
use anyhow::Result; 
use tokio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command as TokioCommand;
use aptos_logger::Logger;
use aptos_language_e2e_tests::{
	account::Account, common_transactions::peer_to_peer_txn, executor::FakeExecutor,
};
use aptos_types::{
	account_config::{DepositEvent, WithdrawEvent},
	transaction::{ExecutionStatus, SignedTransaction, TransactionOutput, TransactionStatus},
};
use std::{
	convert::TryFrom, 
	time::Instant,
	str::FromStr,
	process::{Command, Stdio}
};

use url::Url;

#[tokio::test]
async fn test_movement_client_should_build_and_fetch_accounts() -> Result<(), anyhow::Error> {
	let scaffold: TestHarness = TestHarness::new_with_movement().await;
	let movement_client = scaffold.movement_client().expect("Failed to get MovementClient");
// Todo: Local testnet rather than devnet

	//let mut child = TokioCommand::new("aptos")
        //.args(&["node", "run-local-testnet"])
        //.stdout(Stdio::piped())
        //.stderr(Stdio::piped())
        //.spawn()?;
//
    	//let stdout = child.stdout.take().expect("Failed to capture stdout");
    	//let mut reader = BufReader::new(stdout).lines();
//
    	//while let Some(line) = reader.next_line().await? {
        //	println!("Output: {}", line);
//
        //	if line.contains("Setup is complete") {
      	//      		println!("Testnet is up and running!");
        //		break;
        //	}
	//}

	// let output = Command::new("aptos")
        // .arg("node")
        // .arg("run-local-testnet")
        // .stdout(Stdio::piped())  
        // .spawn()?;  
// 
	// println!("stdout: {}", String::from_utf8_lossy(&output.stdout));

	//let rest_client = &movement_client.rest_client;

	let node_url = format!("https://aptos.devnet.suzuka.movementlabs.xyz/v1/");
	let node_url = Url::from_str(node_url.as_str()).unwrap();
	let faucet_url = format!("https://faucet.devnet.suzuka.movementlabs.xyz");
	let faucet_url = Url::from_str(faucet_url.as_str()).unwrap();
	let rest_client = Client::new(node_url.clone());
	let coin_client = CoinClient::new(&rest_client);
	let faucet_client = FaucetClient::new(faucet_url.clone(), node_url.clone());	
	let mut alice = LocalAccount::generate(&mut rand::rngs::OsRng);
	let bob = LocalAccount::generate(&mut rand::rngs::OsRng); 

	// Print account addresses.
	println!("\n=== Addresses ===");
	println!("Alice: {}", alice.address().to_hex_literal());
	println!("Bob: {}", bob.address().to_hex_literal());
	
	faucet_client
		.fund(alice.address(), 100_000_000)
		.await
		.context("Failed to fund Alice's account")?;
	faucet_client
		.create_account(bob.address())
		.await
		.context("Failed to fund Bob's account")?; 

	// Print initial balances.
	println!("\n=== Initial Balances ===");
	println!(
		"Alice: {:?}",
		coin_client
			.get_account_balance(&alice.address())
			.await
			.context("Failed to get Alice's account balance")?
	);
	println!(
		"Bob: {:?}",
		coin_client
			.get_account_balance(&bob.address())
			.await
			.context("Failed to get Bob's account balance")?
	);

	Ok(())
}

#[tokio::test]
async fn test_eth_client_should_build_and_fetch_accounts() {
	let scaffold: TestHarness = TestHarness::new_only_eth().await;

	let eth_client = scaffold.eth_client().expect("Failed to get EthClient");
	let _anvil = Anvil::new().port(eth_client.rpc_port()).spawn();

	let expected_accounts = vec![
		address!("f39fd6e51aad88f6f4ce6ab8827279cfffb92266"),
		address!("70997970c51812dc3a010c7d01b50e0d17dc79c8"),
		address!("3c44cdddb6a900fa2b585dd299e03d12fa4293bc"),
		address!("90f79bf6eb2c4f870365e785982e1f101e93b906"),
		address!("15d34aaf54267db7d7c367839aaf71a00a2c6a65"),
		address!("9965507d1a55bcc2695c58ba16fb37d819b0a4dc"),
		address!("976ea74026e726554db657fa54763abd0c3a0aa9"),
		address!("14dc79964da2c08b23698b3d3cc7ca32193d9955"),
		address!("23618e81e3f5cdf7f54c3d65f7fbc0abf5b21e8f"),
		address!("a0ee7a142d267c1f36714e4a8f75612f20a79720"),
	];

	let provider = scaffold.eth_client.unwrap().rpc_provider().clone();
	println!("provider: {:?}", provider);
	let accounts = provider.get_accounts().await.expect("Failed to get accounts");
	assert_eq!(accounts.len(), expected_accounts.len());

	for (account, expected) in accounts.iter().zip(expected_accounts.iter()) {
		assert_eq!(account, expected);
	}
}

#[tokio::test]
async fn test_client_should_deploy_initiator_contract() {
	let mut harness: TestHarness = TestHarness::new_only_eth().await;
	let anvil = Anvil::new().port(harness.rpc_port()).spawn();

	let _ = harness.set_eth_signer(anvil.keys()[0].clone());

	let initiator_address = harness.deploy_initiator_contract().await;
	let expected_address = address!("5fbdb2315678afecb367f032d93f642f64180aa3");

	assert_eq!(initiator_address, expected_address);
}

#[tokio::test]
async fn test_client_should_successfully_call_initialize() {
	let mut harness: TestHarness = TestHarness::new_only_eth().await;
	let anvil = Anvil::new().port(harness.rpc_port()).spawn();

	let _ = harness.set_eth_signer(anvil.keys()[0].clone());
	harness.deploy_init_contracts().await;
}

#[tokio::test]
async fn test_client_should_successfully_call_initiate_transfer() {
	let mut harness: TestHarness = TestHarness::new_only_eth().await;
	let anvil = Anvil::new().port(harness.rpc_port()).spawn();

	let signer_address = harness.set_eth_signer(anvil.keys()[0].clone());

	harness.deploy_init_contracts().await;

	let recipient = harness.gen_aptos_account();
	let hash_lock: [u8; 32] = keccak256("secret".to_string().as_bytes()).into();

	harness
		.eth_client_mut()
		.expect("Failed to get EthClient")
		.initiate_bridge_transfer(
			InitiatorAddress(EthAddress(signer_address)),
			RecipientAddress(recipient),
			HashLock(hash_lock),
			TimeLock(100),
			Amount(1000), // Eth
		)
		.await
		.expect("Failed to initiate bridge transfer");
}

#[tokio::test]
#[ignore] // To be tested after this is merged in https://github.com/movementlabsxyz/movement/pull/209
async fn test_client_should_successfully_get_bridge_transfer_id() {
	let mut harness: TestHarness = TestHarness::new_only_eth().await;
	let anvil = Anvil::new().port(harness.rpc_port()).spawn();

	let signer_address = harness.set_eth_signer(anvil.keys()[0].clone());
	harness.deploy_init_contracts().await;

	let recipient = harness.gen_aptos_account();
	let hash_lock: [u8; 32] = keccak256("secret".to_string().as_bytes()).into();

	harness
		.eth_client_mut()
		.expect("Failed to get EthClient")
		.initiate_bridge_transfer(
			InitiatorAddress(EthAddress(signer_address)),
			RecipientAddress(recipient),
			HashLock(hash_lock),
			TimeLock(100),
			Amount(1000), // Eth
		)
		.await
		.expect("Failed to initiate bridge transfer");

	//TODO: Here call get details with the captured event
}

#[tokio::test]
#[ignore] // To be tested after this is merged in https://github.com/movementlabsxyz/movement/pull/209
async fn test_client_should_successfully_complete_transfer() {
	let mut harness: TestHarness = TestHarness::new_only_eth().await;
	let anvil = Anvil::new().port(harness.rpc_port()).spawn();

	let signer_address = harness.set_eth_signer(anvil.keys()[0].clone());
	harness.deploy_init_contracts().await;

	let recipient = address!("70997970c51812dc3a010c7d01b50e0d17dc79c8");
	let recipient_bytes: Vec<u8> = recipient.to_string().as_bytes().to_vec();

	let secret = "secret".to_string();
	let hash_lock = keccak256(secret.as_bytes());
	let hash_lock: [u8; 32] = hash_lock.into();

	let _ = harness
		.eth_client_mut()
		.expect("Failed to get EthClient")
		.initiate_bridge_transfer(
			InitiatorAddress(EthAddress(signer_address)),
			RecipientAddress(recipient_bytes),
			HashLock(hash_lock),
			TimeLock(1000),
			Amount(42),
		)
		.await
		.expect("Failed to initiate bridge transfer");

	//TODO: Here call complete with the id captured from the event
}
