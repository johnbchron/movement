module atomic_bridge::atomic_bridge_initiator {
    use aptos_framework::event::{Self, EventHandle};
    use aptos_framework::account::{Self, Account};
    use aptos_framework::timestamp;
    use aptos_framework::primary_fungible_store;
    use aptos_framework::dispatchable_fungible_asset;
    use aptos_std::aptos_hash;
    use std::signer;
    use std::vector;
    use std::bcs;
    use moveth::moveth;

    /// Constants to represent the state of a bridge transfer
    const INITIALIZED: u8 = 0;
    const COMPLETED: u8 = 1;
    const REFUNDED: u8 = 2;

    /// A struct to hold the details of a bridge transfer
    struct BridgeTransfer has key, store {
        amount: u64,
        originator: address,
        recipient: vector<u8>, // eth address
        hash_lock: vector<u8>,
        time_lock: u64,
        state: u8,
    }

    struct BridgeTransferStore has key, store {
        transfers: vector<BridgeTransfer>,
        nonce: u64,
        bridge_transfer_initiated_events: EventHandle<BridgeTransferInitiatedEvent>,
        bridge_transfer_completed_events: EventHandle<BridgeTransferCompletedEvent>,
        bridge_transfer_refunded_events: EventHandle<BridgeTransferRefundedEvent>,
    }

    /// Event triggered upon initiating a bridge transfer
    struct BridgeTransferInitiatedEvent has store, drop {
        bridge_transfer_id: vector<u8>,
        originator: address,
        recipient: vector<u8>,
        amount: u64,
        hash_lock: vector<u8>,
        time_lock: u64,
    }

    /// Event triggered upon completing a bridge transfer
    struct BridgeTransferCompletedEvent has store, drop {
        bridge_transfer_id: vector<u8>,
        pre_image: vector<u8>,
    }

    /// Event triggered upon refunding a bridge transfer
    struct BridgeTransferRefundedEvent has store, drop {
        bridge_transfer_id: vector<u8>,
    }

    entry fun init_module(account: &signer) {
        let addr = signer::address_of(account);
        if (!exists<BridgeTransferStore>(addr)) {
            move_to(account, BridgeTransferStore {
                transfers: vector::empty<BridgeTransfer>(),
                nonce: 0,
                bridge_transfer_initiated_events: account::new_event_handle<BridgeTransferInitiatedEvent>(account),
                bridge_transfer_completed_events: account::new_event_handle<BridgeTransferCompletedEvent>(account),
                bridge_transfer_refunded_events: account::new_event_handle<BridgeTransferRefundedEvent>(account),
            });
        }
    }

    public fun initiate_bridge_transfer(
        initiator: &signer,
        recipient: vector<u8>, // eth address
        hash_lock: vector<u8>,
        time_lock: u64,
        amount: u64
    ): vector<u8> acquires BridgeTransferStore {
        let addr = signer::address_of(initiator);
        let asset = moveth::metadata();
        let store = borrow_global_mut<BridgeTransferStore>(addr);
        if (amount > 0) {
            // Transfer amount of moveth from initiator to contract address
            let initiator_store = primary_fungible_store::ensure_primary_store_exists(addr, asset);
            let bridge_store = primary_fungible_store::ensure_primary_store_exists(@atomic_bridge, asset);
            dispatchable_fungible_asset::transfer(initiator, initiator_store, bridge_store, amount);
        };
        store.nonce = store.nonce + 1;

        // Create a single byte vector by concatenating all components
        let combined_bytes = vector::empty<u8>();
        vector::append(&mut combined_bytes, bcs::to_bytes(&addr));
        vector::append(&mut combined_bytes, recipient);
        vector::append(&mut combined_bytes, hash_lock);
        vector::append(&mut combined_bytes, bcs::to_bytes(&store.nonce));

        let bridge_transfer_id = aptos_hash::keccak256(combined_bytes);

        let bridge_transfer = BridgeTransfer {
            amount: amount,
            originator: addr,
            recipient: recipient,
            hash_lock: hash_lock, 
            time_lock: timestamp::now_seconds() + time_lock,
            state: INITIALIZED,
        };

        vector::push_back(&mut store.transfers, bridge_transfer);

        event::emit_event(&mut store.bridge_transfer_initiated_events, BridgeTransferInitiatedEvent {
            bridge_transfer_id: bridge_transfer_id,
            originator: addr,
            recipient: recipient,
            amount: amount,
            hash_lock: hash_lock,
            time_lock: timestamp::now_seconds() + time_lock,
        });

        bridge_transfer_id
    }

    public fun complete_bridge_transfer(
        account: &signer,
        bridge_transfer_id: vector<u8>,
        pre_image: vector<u8>,
        master_minter: &signer
    ) acquires BridgeTransferStore {
        let addr = signer::address_of(account);
        let store = borrow_global_mut<BridgeTransferStore>(addr);
        let idx = get_bridge_transfer_index(&store.transfers, &bridge_transfer_id);
        let bridge_transfer = vector::borrow_mut(&mut store.transfers, idx);

        assert!(bridge_transfer.state == INITIALIZED, 1);
        assert!(aptos_hash::keccak256(bcs::to_bytes(&pre_image)) == bridge_transfer.hash_lock, 2);
        assert!(timestamp::now_seconds() <= bridge_transfer.time_lock, 3);

        moveth::add_minter(master_minter, signer::address_of(account));

        moveth::burn(account, @atomic_bridge, bridge_transfer.amount); 

        bridge_transfer.state = COMPLETED;

        event::emit_event(&mut store.bridge_transfer_completed_events, BridgeTransferCompletedEvent {
            bridge_transfer_id: bridge_transfer_id,
            pre_image: pre_image,
        });
    }

    public fun refund_bridge_transfer(
        account: &signer, 
        bridge_transfer_id: vector<u8>,
        atomic_bridge: &signer
    ) acquires BridgeTransferStore {
        let addr = signer::address_of(account);
        let store = borrow_global_mut<BridgeTransferStore>(addr);
        let idx = get_bridge_transfer_index(&store.transfers, &bridge_transfer_id);
        let bridge_transfer = vector::borrow_mut(&mut store.transfers, idx);

        assert!(bridge_transfer.state == INITIALIZED, 1);
        assert!(timestamp::now_seconds() > bridge_transfer.time_lock, 2);

        // moveth::add_minter(master_minter, signer::address_of(account));
        // moveth::mint(account, bridge_transfer.originator, bridge_transfer.amount);
        
        let initiator_addr = bridge_transfer.originator;
        let bridge_addr = signer::address_of(atomic_bridge);
        let asset = moveth::metadata();

        let initiator_store = primary_fungible_store::ensure_primary_store_exists(initiator_addr, asset);
        let bridge_store = primary_fungible_store::ensure_primary_store_exists(@atomic_bridge, asset);
        dispatchable_fungible_asset::transfer(atomic_bridge, bridge_store, initiator_store, bridge_transfer.amount);

        bridge_transfer.state = REFUNDED;

        event::emit_event(&mut store.bridge_transfer_refunded_events, BridgeTransferRefundedEvent {
            bridge_transfer_id: bridge_transfer_id,
        });
    }

    /// Helper function to find the index of a bridge transfer in the transfers vector
    public fun get_bridge_transfer_index(transfers: &vector<BridgeTransfer>, bridge_transfer_id: &vector<u8>): u64 {
        let len = vector::length(transfers);
        let i = 0;
        while (i < len) {
            let transfer = vector::borrow(transfers, i);

            // Create a single byte vector by concatenating all components
            let combined_bytes = vector::empty<u8>();
            vector::append(&mut combined_bytes, bcs::to_bytes(&transfer.originator));
            vector::append(&mut combined_bytes, transfer.recipient);
            vector::append(&mut combined_bytes, transfer.hash_lock);
            vector::append(&mut combined_bytes, bcs::to_bytes(&i));
            
            let id = aptos_hash::keccak256(combined_bytes);
            if (id == *bridge_transfer_id) {
                break
            };
            i = i + 1;
        };
        i - 1
    }

    #[test(sender = @0xdaff)]
    public fun test_initialize (
        sender: signer
    ) acquires BridgeTransferStore{
        let addr = signer::address_of(&sender);

        // Ensure Account resource exists for the sender
        account::create_account_if_does_not_exist(addr);

        init_module(&sender);

        assert!(exists<BridgeTransferStore>(addr), 999);

        let addr = signer::address_of(&sender);
        let store = borrow_global<BridgeTransferStore>(addr);

        assert!(vector::length(&store.transfers) == 0, 100);
        assert!(store.nonce == 0, 101);
    }

    #[test(creator = @moveth, aptos_framework = @0x1, sender = @0xdaff, master_minter = @master_minter)]
    public fun test_initiate_bridge_transfer(
        sender: &signer,
        creator: &signer,
        aptos_framework: &signer,
        master_minter: &signer
    ) acquires BridgeTransferStore{
        moveth::init_for_test(creator);
        timestamp::set_time_has_started_for_testing(aptos_framework);
        let addr = signer::address_of(sender);
        // Ensure Account resource exists for the sender
        account::create_account_if_does_not_exist(addr);

        init_module(sender);

        let recipient = b"recipient_address";
        let hash_lock = b"hash_lock_value";
        let time_lock = 1000;
        let amount = 1000;

        // Mint amount of tokens to sender
        let sender_address = signer::address_of(sender);
        moveth::mint(master_minter, sender_address, amount);
        
        let bridge_transfer_id = initiate_bridge_transfer(
            sender,
            recipient,
            hash_lock,
            time_lock,
            amount
        );

        let addr = signer::address_of(sender);
        let store = borrow_global<BridgeTransferStore>(addr);
        let idx = get_bridge_transfer_index(&store.transfers, &bridge_transfer_id);
        let transfer = vector::borrow(&store.transfers, idx);

        assert!(transfer.amount == amount, 200);
        assert!(transfer.originator == addr, 201);
        assert!(transfer.recipient == b"recipient_address", 202);
        assert!(transfer.hash_lock == b"hash_lock_value", 203);
        assert!(transfer.time_lock == timestamp::now_seconds() + time_lock, 204);
        assert!(transfer.state == INITIALIZED, 205);
    }

    #[test(creator = @moveth, aptos_framework = @0x1, sender = @0xdaff, master_minter = @master_minter)]
    #[expected_failure]
    public fun test_initiate_bridge_transfer_no_moveth(
        sender: &signer,
        creator: &signer,
        aptos_framework: &signer,
        master_minter: &signer
    ) acquires BridgeTransferStore{
        moveth::init_for_test(creator);
        timestamp::set_time_has_started_for_testing(aptos_framework);
        let addr = signer::address_of(sender);
        // Ensure Account resource exists for the sender
        account::create_account_if_does_not_exist(addr);

        init_module(sender);

        let recipient = b"recipient_address";
        let hash_lock = b"hash_lock_value";
        let time_lock = 1000;
        let amount = 1000;

        // Do not mint tokens to sender; sender has no MovETH
        
        let bridge_transfer_id = initiate_bridge_transfer(
            sender,
            recipient,
            hash_lock,
            time_lock,
            amount
        );

        let addr = signer::address_of(sender);
        let store = borrow_global<BridgeTransferStore>(addr);
        let idx = get_bridge_transfer_index(&store.transfers, &bridge_transfer_id);
        let transfer = vector::borrow(&store.transfers, idx);

        assert!(transfer.amount == amount, 200);
        assert!(transfer.originator == addr, 201);
        assert!(transfer.recipient == b"recipient_address", 202);
        assert!(transfer.hash_lock == b"hash_lock_value", 203);
        assert!(transfer.time_lock == timestamp::now_seconds() + time_lock, 204);
        assert!(transfer.state == INITIALIZED, 205);
    }

    #[test(creator = @moveth, aptos_framework = @0x1, sender = @0xdaff, master_minter = @master_minter)]
    public fun test_complete_bridge_transfer(
        sender: &signer,
        master_minter: &signer,
        creator: &signer,
        aptos_framework: &signer    
    ) acquires BridgeTransferStore{
        moveth::init_for_test(creator);
        timestamp::set_time_has_started_for_testing(aptos_framework);
        let addr = signer::address_of(sender);
        // Ensure Account resource exists for the sender
        account::create_account_if_does_not_exist(addr);
        init_module(sender);

        assert!(exists<BridgeTransferStore>(addr), 42);
        let recipient = b"recipient_address";
        let pre_image = b"pre_image_value";
        let hash_lock = aptos_hash::keccak256(bcs::to_bytes(&pre_image));
        assert!(aptos_hash::keccak256(bcs::to_bytes(&pre_image)) == hash_lock, 5);
        let time_lock = 1000;
        let amount = 1000;
        let sender_address = signer::address_of(sender);
        moveth::mint(master_minter, sender_address, amount);
        
        let bridge_transfer_id = initiate_bridge_transfer(
            sender,
            recipient,
            hash_lock,
            time_lock,
            amount
        );

        complete_bridge_transfer(
            sender,
            bridge_transfer_id,
            pre_image,
            master_minter
        );

        let addr = signer::address_of(sender);
        let store = borrow_global<BridgeTransferStore>(addr);
        let idx = get_bridge_transfer_index(&store.transfers, &bridge_transfer_id);
        let transfer = vector::borrow(&store.transfers, idx);

        assert!(transfer.state == COMPLETED, 300);
    }

    #[test(creator = @moveth, aptos_framework = @0x1, sender = @0xdaff, master_minter = @master_minter)]
    #[expected_failure]
    public fun test_complete_bridge_transfer_wrong_preimage(
        sender: &signer,
        master_minter: &signer,
        creator: &signer,
        aptos_framework: &signer    
    ) acquires BridgeTransferStore{
        moveth::init_for_test(creator);
        timestamp::set_time_has_started_for_testing(aptos_framework);
        let addr = signer::address_of(sender);
        // Ensure Account resource exists for the sender
        account::create_account_if_does_not_exist(addr);
        init_module(sender);

        assert!(exists<BridgeTransferStore>(addr), 42);
        let recipient = b"recipient_address";
        let pre_image = b"pre_image_value";
        let wrong_pre_image = b"wrong_pre_image_value";
        let hash_lock = aptos_hash::keccak256(bcs::to_bytes(&pre_image));
        let time_lock = 1000;
        let amount = 1000;
        let sender_address = signer::address_of(sender);
        moveth::mint(master_minter, sender_address, amount);
        
        let bridge_transfer_id = initiate_bridge_transfer(
            sender,
            recipient,
            hash_lock,
            time_lock,
            amount
        );

        complete_bridge_transfer(
            sender,
            bridge_transfer_id,
            wrong_pre_image,
            master_minter
        );

        let addr = signer::address_of(sender);
        let store = borrow_global<BridgeTransferStore>(addr);
        let idx = get_bridge_transfer_index(&store.transfers, &bridge_transfer_id);
        let transfer = vector::borrow(&store.transfers, idx);

        assert!(transfer.state == COMPLETED, 300);
    }

    #[test(creator = @moveth, aptos_framework = @0x1, sender = @0xdaff, atomic_bridge = @atomic_bridge)]
    public fun test_refund_bridge_transfer(
        sender: &signer,
        atomic_bridge: &signer,
        creator: &signer,
        aptos_framework: &signer
    ) acquires BridgeTransferStore{
        moveth::init_for_test(creator);
        let asset = moveth::metadata();
        timestamp::set_time_has_started_for_testing(aptos_framework);

        let addr = signer::address_of(sender);
        // Ensure Account resource exists for the sender
        account::create_account_if_does_not_exist(addr);
        init_module(sender);

        let recipient = b"recipient_address";
        let hash_lock = b"hash_lock_value";
        let time_lock = 1;
        let amount = 1000;

        // Mint amount of tokens to sender
        let sender_address = signer::address_of(sender);
        moveth::mint(atomic_bridge, sender_address, amount);

        let bridge_transfer_id = initiate_bridge_transfer(
            sender,
            recipient,
            hash_lock,
            time_lock,
            amount
        );

        // Simulate time passing
        aptos_framework::timestamp::fast_forward_seconds(time_lock + 1);

        refund_bridge_transfer(
            sender,
            bridge_transfer_id,
            atomic_bridge
        );
        
        let addr = signer::address_of(sender);
        assert!(primary_fungible_store::balance(addr, asset) == amount, 0);

        let store = borrow_global<BridgeTransferStore>(addr);
        let idx = get_bridge_transfer_index(&store.transfers, &bridge_transfer_id);
        let transfer = vector::borrow(&store.transfers, idx);

        assert!(transfer.state == REFUNDED, 400);
    }


}
