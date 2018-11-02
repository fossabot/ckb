use bigint::{H256, U256};
use ckb_chain::chain::{Chain, ChainBuilder, ChainProvider};
use ckb_chain::consensus::Consensus;
use ckb_chain::store::ChainKVStore;
use ckb_notify::Notify;
use ckb_protocol::RelayMessage;
use ckb_time::now_ms;
use core::block::BlockBuilder;
use core::header::HeaderBuilder;
use core::script::Script;
use core::transaction::{CellInput, CellOutput, OutPoint, TransactionBuilder};
use db::memorydb::MemoryKeyValueDB;
use flatbuffers::get_root;
use flatbuffers::FlatBufferBuilder;
use pool::{PoolConfig, TransactionPool};
use relayer::TX_PROPOSAL_TOKEN;
use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::sync::mpsc::channel;
use std::sync::{Arc, Barrier};
use std::thread;
use tests::{dummy_pow_engine, TestNode};
use {Relayer, RELAY_PROTOCOL_ID};

#[test]
fn relay_compact_block_with_one_tx() {
    let (mut node1, chain1) = setup_node(3);
    let (mut node2, chain2) = setup_node(3);
    let barrier = Arc::new(Barrier::new(2));

    node1.connect(&mut node2, RELAY_PROTOCOL_ID);

    let (signal_tx1, _) = channel();
    let barrier1 = Arc::clone(&barrier);
    thread::spawn(move || {
        let last_block = chain1.block(&chain1.tip_header().read().hash()).unwrap();
        let last_cellbase = last_block.commit_transactions().first().unwrap();

        // building tx and broadcast it
        let tx = TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(last_cellbase.hash(), 0),
                create_valid_script(),
            )).output(CellOutput::new(50, Vec::new(), H256::zero()))
            .build();

        {
            let fbb = &mut FlatBufferBuilder::new();
            let message = RelayMessage::build_transaction(fbb, &tx);
            fbb.finish(message, None);
            node1.broadcast(RELAY_PROTOCOL_ID, fbb.finished_data().to_vec());
        }

        // building 1st compact block with tx proposal and broadcast it
        let block = {
            let number = last_block.header().number() + 1;
            let timestamp = last_block.header().timestamp() + 1;
            let difficulty = chain1.calculate_difficulty(&last_block.header()).unwrap();
            let cellbase = TransactionBuilder::default()
                .input(CellInput::new_cellbase_input(number))
                .output(CellOutput::default())
                .build();

            let header_builder = HeaderBuilder::default()
                .parent_hash(&last_block.header().hash())
                .number(number)
                .timestamp(timestamp)
                .difficulty(&difficulty)
                .cellbase_id(&cellbase.hash());

            BlockBuilder::default()
                .commit_transaction(cellbase)
                .proposal_transaction(tx.proposal_short_id())
                .with_header_builder(header_builder)
        };

        {
            chain1
                .process_block(&block)
                .expect("process block should be OK");

            let fbb = &mut FlatBufferBuilder::new();
            let message = RelayMessage::build_compact_block(fbb, &block, &HashSet::new());
            fbb.finish(message, None);
            node1.broadcast(RELAY_PROTOCOL_ID, fbb.finished_data().to_vec());
        }

        // building 2nd compact block with tx and broadcast it
        let last_block = block;

        let block = {
            let number = last_block.header().number() + 1;
            let timestamp = last_block.header().timestamp() + 1;
            let difficulty = chain1.calculate_difficulty(&last_block.header()).unwrap();
            let cellbase = TransactionBuilder::default()
                .input(CellInput::new_cellbase_input(number))
                .output(CellOutput::default())
                .build();

            let header_builder = HeaderBuilder::default()
                .parent_hash(&last_block.header().hash())
                .number(number)
                .timestamp(timestamp)
                .difficulty(&difficulty)
                .cellbase_id(&cellbase.hash());

            BlockBuilder::default()
                .commit_transaction(cellbase)
                .commit_transaction(tx)
                .with_header_builder(header_builder)
        };

        {
            chain1
                .process_block(&block)
                .expect("process block should be OK");

            let fbb = &mut FlatBufferBuilder::new();
            let message = RelayMessage::build_compact_block(fbb, &block, &HashSet::new());
            fbb.finish(message, None);
            node1.broadcast(RELAY_PROTOCOL_ID, fbb.finished_data().to_vec());
        }

        node1.start(signal_tx1, |_| false);
        barrier1.wait();
    });

    let barrier2 = Arc::clone(&barrier);
    let (signal_tx2, signal_rx2) = channel();
    thread::spawn(move || {
        node2.start(signal_tx2, |data| {
            let msg = get_root::<RelayMessage>(data);
            // terminate thread 2 compact block
            msg.payload_as_compact_block()
                .map(|block| block.header().unwrap().number() == 5)
                .unwrap_or(false)
        });
        barrier2.wait();
    });

    // Wait node2 receive transaction and block from node1
    let _ = signal_rx2.recv();

    assert_eq!(chain2.tip_header().read().number(), 5);
}

#[test]
fn relay_compact_block_with_missing_indexs() {
    let (mut node1, chain1) = setup_node(3);
    let (mut node2, chain2) = setup_node(3);

    node1.connect(&mut node2, RELAY_PROTOCOL_ID);

    let (signal_tx1, _) = channel();
    thread::spawn(move || {
        let last_block = chain1.block(&chain1.tip_header().read().hash()).unwrap();
        let last_cellbase = last_block.commit_transactions().first().unwrap();

        // building 10 txs and broadcast some
        let txs = (0..10u8)
            .map(|i| {
                TransactionBuilder::default()
                    .input(CellInput::new(
                        OutPoint::new(last_cellbase.hash(), i as u32),
                        create_valid_script(),
                    )).output(CellOutput::new(50, vec![i], H256::zero()))
                    .build()
            }).collect::<Vec<_>>();

        [3, 5].iter().for_each(|i| {
            let fbb = &mut FlatBufferBuilder::new();
            let message = RelayMessage::build_transaction(fbb, txs.get(*i).unwrap());
            fbb.finish(message, None);
            node1.broadcast(RELAY_PROTOCOL_ID, fbb.finished_data().to_vec());
        });

        // building 1st compact block with tx proposal and broadcast it
        let block = {
            let number = last_block.header().number() + 1;
            let timestamp = last_block.header().timestamp() + 1;
            let difficulty = chain1.calculate_difficulty(&last_block.header()).unwrap();
            let cellbase = TransactionBuilder::default()
                .input(CellInput::new_cellbase_input(number))
                .output(CellOutput::default())
                .build();

            let header_builder = HeaderBuilder::default()
                .parent_hash(&last_block.header().hash())
                .number(number)
                .timestamp(timestamp)
                .difficulty(&difficulty)
                .cellbase_id(&cellbase.hash());

            BlockBuilder::default()
                .commit_transaction(cellbase)
                .proposal_transactions(txs.iter().map(|tx| tx.proposal_short_id()).collect())
                .with_header_builder(header_builder)
        };

        {
            chain1
                .process_block(&block)
                .expect("process block should be OK");

            let fbb = &mut FlatBufferBuilder::new();
            let message = RelayMessage::build_compact_block(fbb, &block, &HashSet::new());
            fbb.finish(message, None);
            node1.broadcast(RELAY_PROTOCOL_ID, fbb.finished_data().to_vec());
        }

        // building 2nd compact block with txs and broadcast it
        let last_block = block;

        let block = {
            let number = last_block.header().number() + 1;
            let timestamp = last_block.header().timestamp() + 1;
            let difficulty = chain1.calculate_difficulty(&last_block.header()).unwrap();
            let cellbase = TransactionBuilder::default()
                .input(CellInput::new_cellbase_input(number))
                .output(CellOutput::default())
                .build();

            let header_builder = HeaderBuilder::default()
                .parent_hash(&last_block.header().hash())
                .number(number)
                .timestamp(timestamp)
                .difficulty(&difficulty)
                .cellbase_id(&cellbase.hash());

            BlockBuilder::default()
                .commit_transaction(cellbase)
                .commit_transactions(txs)
                .with_header_builder(header_builder)
        };

        {
            chain1
                .process_block(&block)
                .expect("process block should be OK");

            let fbb = &mut FlatBufferBuilder::new();
            let message = RelayMessage::build_compact_block(fbb, &block, &HashSet::new());
            fbb.finish(message, None);
            node1.broadcast(RELAY_PROTOCOL_ID, fbb.finished_data().to_vec());
        }

        node1.start(signal_tx1, |_| false);
    });

    let (signal_tx2, signal_rx2) = channel();
    thread::spawn(move || {
        node2.start(signal_tx2, |data| {
            let msg = get_root::<RelayMessage>(data);
            // terminate thread after processing block transactions
            msg.payload_as_block_transactions()
                .map(|_| true)
                .unwrap_or(false)
        });
    });

    // Wait node2 receive transaction and block from node1
    let _ = signal_rx2.recv();

    assert_eq!(chain2.tip_header().read().number(), 5);
}

fn setup_node(height: u64) -> (TestNode, Arc<Chain<ChainKVStore<MemoryKeyValueDB>>>) {
    let mut block = BlockBuilder::default().with_header_builder(
        HeaderBuilder::default()
            .timestamp(now_ms())
            .difficulty(&U256::from(1000)),
    );

    let notify = Notify::new();

    let consensus = Consensus::default().set_genesis_block(block.clone());
    let builder = ChainBuilder::<ChainKVStore<MemoryKeyValueDB>>::new_memory()
        .consensus(consensus.clone())
        .notify(notify.clone());
    let chain = Arc::new(builder.build().unwrap());

    for _i in 0..height {
        let number = block.header().number() + 1;
        let timestamp = block.header().timestamp() + 1;
        let difficulty = chain.calculate_difficulty(&block.header()).unwrap();
        let outputs = (0..20)
            .map(|_| CellOutput::new(50, Vec::new(), create_valid_script().redeem_script_hash()))
            .collect::<Vec<_>>();
        let cellbase = TransactionBuilder::default()
            .input(CellInput::new_cellbase_input(number))
            .outputs(outputs)
            .build();

        let header_builder = HeaderBuilder::default()
            .parent_hash(&block.header().hash())
            .number(number)
            .timestamp(timestamp)
            .difficulty(&difficulty)
            .cellbase_id(&cellbase.hash());

        block = BlockBuilder::default()
            .commit_transaction(cellbase)
            .with_header_builder(header_builder);

        chain
            .process_block(&block)
            .expect("process block should be OK");
    }

    let tx_pool = TransactionPool::new(PoolConfig::default(), Arc::clone(&chain), notify);
    let relayer = Relayer::new(&chain, &dummy_pow_engine(), &tx_pool);

    let mut node = TestNode::default();
    node.add_protocol(
        RELAY_PROTOCOL_ID,
        Arc::new(relayer),
        vec![TX_PROPOSAL_TOKEN],
    );
    (node, chain)
}

// This helper is copied from pool test
// TODO should provide some helper or add validation option to pool / chain for testing
fn create_valid_script() -> Script {
    let mut file = File::open("../spec/res/cells/always_success").unwrap();
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer).unwrap();

    Script::new(0, Vec::new(), None, Some(buffer), Vec::new())
}