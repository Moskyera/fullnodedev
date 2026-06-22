#![no_main]

use libfuzzer_sys::fuzz_target;
use protocol::tex::TexCellAct;

fuzz_target!(|data: &[u8]| {
    let mut tex = TexCellAct::new();
    let _ = tex.parse(data);
});