//! A fidelity bond is never built on a coin that can still be replaced.
//!
//! A pending bond is rebroadcast as the same bytes until it confirms. If it
//! spent an unconfirmed deposit and that deposit were then replaced (a fee
//! bump), the bond could never be valid again and setup would wait on it
//! forever, restarts included. The maker must wait for the deposit to confirm.

use bitcoin::{
    absolute::LockTime, transaction::Version, Address, Amount, OutPoint, Sequence, Transaction,
    TxIn, TxOut, Txid, Witness,
};
use bitcoind::{bitcoincore_rpc::RpcApi, BitcoinD};
use openswap::{maker::start_server, wallet::AddressType};

use crate::test_framework::*;

use std::{sync::atomic::Ordering::Relaxed, thread, time::Duration};

/// A coin of the regtest node's own wallet worth at least 2 BTC.
fn node_coin(bitcoind: &BitcoinD) -> (OutPoint, Amount) {
    let utxo = bitcoind
        .client
        .list_unspent(Some(101), None, None, None, None)
        .unwrap()
        .into_iter()
        .find(|utxo| utxo.amount >= Amount::from_btc(2.0).unwrap())
        .expect("the node wallet holds a mature coin");
    (OutPoint::new(utxo.txid, utxo.vout), utxo.amount)
}

/// Pays 1 BTC from `coin` to `to` at `fee`, with the change back to the node,
/// and broadcasts it. A second call on the same coin conflicts with the
/// first and, with a higher fee, replaces it.
fn deposit(bitcoind: &BitcoinD, coin: (OutPoint, Amount), to: &Address, fee: Amount) -> Txid {
    let change = bitcoind
        .client
        .get_raw_change_address(None)
        .unwrap()
        .assume_checked();
    let tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: coin.0,
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            script_sig: Default::default(),
            witness: Witness::new(),
        }],
        output: vec![
            TxOut {
                value: Amount::ONE_BTC,
                script_pubkey: to.script_pubkey(),
            },
            TxOut {
                value: coin.1 - Amount::ONE_BTC - fee,
                script_pubkey: change.script_pubkey(),
            },
        ],
    };
    let signed = bitcoind
        .client
        .sign_raw_transaction_with_wallet(&tx, None, None)
        .unwrap();
    assert!(signed.complete, "the node wallet signs its own coin");
    bitcoind
        .client
        .send_raw_transaction(&signed.transaction().unwrap())
        .unwrap()
}

/// A maker whose deposit is fee-bumped before it confirms waits for the
/// replacement and builds its bond on that, instead of on the deposit that
/// is gone.
#[world_test(
    makers = 1,
    cases = [
        bond_waits_for_a_replaced_deposit(backend = BitcoindBackend),
        bond_waits_for_a_replaced_deposit_electrum(backend = ElectrumBackend),
    ],
)]
fn run_bond_waits_for_a_replaced_deposit(world: &mut World) {
    let bitcoind = world.bitcoind();
    let maker = world.makers()[0].inner();

    // Nothing confirms until the replacement is in the mempool.
    world.framework().set_block_gen_paused(true);

    let maker_clone = maker.clone();
    let maker_thread = thread::spawn(move || {
        let _ = start_server(maker_clone);
    });
    wait_logged!(world, "Send at least", Duration::from_secs(120));

    let address = maker
        .wallet
        .write()
        .unwrap()
        .get_next_external_address(AddressType::P2TR)
        .unwrap();
    let coin = node_coin(bitcoind);
    let original = deposit(bitcoind, coin, &address, Amount::from_sat(500));

    // The maker sees the deposit in the mempool and must hold off, not build
    // its bond on top of it.
    wait_for_new_log(
        &world.taker_log_path(),
        "of unconfirmed funds to confirm before creating the fidelity bond",
        Duration::from_secs(60),
    );
    assert_log!(world; { lacks "Fidelity bond broadcast, waiting for confirmation" });

    // The fee bump: same coin, same payment, higher fee.
    let replacement = deposit(bitcoind, coin, &address, Amount::from_sat(100_000));
    assert!(
        bitcoind.client.get_mempool_entry(&original).is_err(),
        "the first deposit must be replaced"
    );
    assert!(
        bitcoind.client.get_mempool_entry(&replacement).is_ok(),
        "the replacement must be in the mempool"
    );
    world.framework().set_block_gen_paused(false);
    world.mine(1);

    wait_for_makers_setup(std::slice::from_ref(maker), 120);
    maker.shutdown.store(true, Relaxed);
    let _ = maker_thread.join();

    let bond_txid = {
        let wallet = maker.wallet.read().unwrap();
        let bonds = wallet.get_fidelity_bonds();
        assert_eq!(bonds.len(), 1, "exactly one bond must be built");
        bonds[0].outpoint().txid
    };
    let bond = bitcoind
        .client
        .get_raw_transaction(&bond_txid, None)
        .unwrap();
    assert!(
        bond.input
            .iter()
            .all(|input| input.previous_output.txid == replacement),
        "the bond must spend the confirmed replacement, not {}",
        original
    );
    assert_log!(world; {
        count("Fidelity bond broadcast, waiting for confirmation") == 1,
        lacks "Could not check fidelity bond",
    });
}
