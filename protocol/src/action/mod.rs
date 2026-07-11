use std::any::*;
use std::collections::*;

use basis::component::*;
use basis::interface::*;
use field::*;
use sys::*;

use super::context::*;
use super::operate::*;
use super::setup::*;
use super::state::*;

include! {"macro.rs"}
include! {"level.rs"}
include! {"create.rs"}

include! {"hacash.rs"}
include! {"satoshi.rs"}
include! {"diamond.rs"}
include! {"asset.rs"}
// include!{"diamond_mint.rs"}
// include!{"diamond_insc.rs"}
// include!{"diamond_util.rs"}
// include!{"channel.rs"}
include! {"blob.rs"}
include! {"chain.rs"}
include! {"reqsign.rs"}

include! {"asthelper.rs"}
include! {"astselect.rs"}
include! {"astif.rs"}

/*
* register
*/
pub fn register(setup: &mut ProtocolSetup) {
    setup.action_codec(ACTION_CODEC_KINDS, try_create, try_json_decode)
}

action_register! {

    // hac
    HacToTrs
    HacFromTrs
    HacFromToTrs
    // HacAmountCompress

    DiaSingleTrs
    DiaFromToTrs
    DiaToTrs
    DiaFromTrs

    // satoshi
    // SatoshiGenesis
    SatToTrs
    SatFromTrs
    SatFromToTrs

    // asset
    // AssetCreate
    AssetToTrs
    AssetFromTrs
    AssetFromToTrs

    AstSelect
    AstIf

    TxMessage
    TxBlob

    HeightScope
    ChainAllow
    BalanceFloor

    ReqSignList


}
