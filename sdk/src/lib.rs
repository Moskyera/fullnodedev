// #![no_std]
#![cfg_attr(all(target_arch = "wasm32", not(test)), no_main)]

// use wasm_bindgen::prelude::wasm_bindgen;

// #[panic_handler]
// fn handle_panic(_: &core::panic::PanicInfo) -> ! {
//     loop {}
// }

use field::*;
use sys::Account as SysAccount;
use sys::*;
use wasm_bindgen::prelude::*;

include! {"param.rs"}
include! {"util.rs"}
include! {"keystore.rs"}
include! {"hybrid.rs"}
include! {"account.rs"}
include! {"coin.rs"}
include! {"sign.rs"}
