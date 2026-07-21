use basis::component::*;
use basis::interface::*;
use field::*;
use sys::*;

use super::action::*;
use super::context::*;
use super::operate;
use super::state::*;
use super::*;

include! {"util.rs"}
include! {"macro.rs"}
include! {"type3.rs"}
include! {"hybrid_util.rs"}
include! {"type4.rs"}
include! {"prelude.rs"}
include! {"create.rs"}
include! {"store.rs"}

/*
* define
*/
transaction_define_legacy! { TransactionType1, 1u8 }
transaction_define_legacy! { TransactionType2, 2u8 }

fn create_default_prelude_tx(buf: &[u8]) -> Ret<(Box<dyn Transaction>, usize)> {
    let (tx, sk) = DefaultPreludeTx::create(buf)?;
    Ok((Box::new(tx), sk))
}

fn decode_default_prelude_tx(json: &str) -> Ret<Box<dyn Transaction>> {
    let mut tx = DefaultPreludeTx::default();
    tx.from_json(json)?;
    Ok(Box::new(tx))
}

fn create_tx_type1(buf: &[u8]) -> Ret<(Box<dyn Transaction>, usize)> {
    let (tx, sk) = TransactionType1::create(buf)?;
    Ok((Box::new(tx), sk))
}

fn decode_tx_type1(json: &str) -> Ret<Box<dyn Transaction>> {
    let mut tx = TransactionType1::default();
    tx.from_json(json)?;
    Ok(Box::new(tx))
}

fn create_tx_type2(buf: &[u8]) -> Ret<(Box<dyn Transaction>, usize)> {
    let (tx, sk) = TransactionType2::create(buf)?;
    Ok((Box::new(tx), sk))
}

fn decode_tx_type2(json: &str) -> Ret<Box<dyn Transaction>> {
    let mut tx = TransactionType2::default();
    tx.from_json(json)?;
    Ok(Box::new(tx))
}

fn create_tx_type3(buf: &[u8]) -> Ret<(Box<dyn Transaction>, usize)> {
    let (tx, sk) = TransactionType3::create(buf)?;
    Ok((Box::new(tx), sk))
}

fn decode_tx_type3(json: &str) -> Ret<Box<dyn Transaction>> {
    let mut tx = TransactionType3::default();
    tx.from_json(json)?;
    Ok(Box::new(tx))
}

fn create_tx_type4(buf: &[u8]) -> Ret<(Box<dyn Transaction>, usize)> {
    let (tx, sk) = TransactionType4::create(buf)?;
    Ok((Box::new(tx), sk))
}

fn decode_tx_type4(json: &str) -> Ret<Box<dyn Transaction>> {
    let mut tx = TransactionType4::default();
    tx.from_json(json)?;
    Ok(Box::new(tx))
}

pub fn register(setup: &mut crate::setup::ProtocolSetup) {
    setup.tx_codec(
        DefaultPreludeTx::TYPE,
        create_default_prelude_tx,
        decode_default_prelude_tx,
    );
    setup.tx_codec(TransactionType1::TYPE, create_tx_type1, decode_tx_type1);
    setup.tx_codec(TransactionType2::TYPE, create_tx_type2, decode_tx_type2);
    setup.tx_codec(TransactionType3::TYPE, create_tx_type3, decode_tx_type3);
    setup.tx_codec(TransactionType4::TYPE, create_tx_type4, decode_tx_type4);
}

/*
// Trs list
*/
combi_dynvec! { DynVecTransaction,
    Uint4, Transaction, transaction_create, transaction_json_decode
}
