//! OpenCL diamond mining kernel path (submodule of diaworker).

use field::{Address, DiamondName, DiamondNumber, Fixed8, Hash};
use mint::action::DIAMOND_ABOVE_NUMBER_OF_CREATE_BY_CUSTOM_MESSAGE;
use mint::action::DiamondMint;
use x16rs::calculate_hash;
use x16rs::diamond_hash;

use crate::hash_util::diamond_more_power;
use crate::opencl_gpu::{
    OpenCLResources, enqueue_diamond_kernel, read_diamond_gpu_results, write_stuff_to_gpu,
};

use super::{DIAMOND_HASH_LEN, DiamondMiningResult, HASH_WIDTH, check_diamer_success};

pub(crate) fn do_diamond_group_mining_opencl(
    opencl: &OpenCLResources,
    number: u32,
    prevblockhash: &Hash,
    rwdaddr: &Address,
    custom_message: &Hash,
    nonce_start: u64,
    nonce_space: u64,
    num_work_groups: u32,
    local_work_size: u32,
    unit_size: u32,
) -> DiamondMiningResult {
    let empthbytes = [0u8; 0];
    let prevhash: &[u8; HASH_WIDTH] = prevblockhash;
    let address: &[u8; 21] = rwdaddr;
    let custom_nonce: &[u8] = match number > DIAMOND_ABOVE_NUMBER_OF_CREATE_BY_CUSTOM_MESSAGE {
        true => custom_message.as_bytes(),
        false => &empthbytes,
    };
    let mut most = DiamondMiningResult {
        number,
        nonce_start,
        nonce_space,
        u64_nonce: 0,
        msg_nonce: custom_nonce.to_vec(),
        dia_str: [b'W'; DIAMOND_HASH_LEN],
        is_success: None,
        use_secs: 0.0,
        is_gpu: true,
        gpu_batch_ok: false,
    };
    let repeat = x16rs::mine_diamond_hash_repeat(number) as u32;
    let stuff = [
        prevhash.to_vec(),
        [0u8; 8].to_vec(),
        address.to_vec(),
        custom_nonce.as_ref().to_vec(),
    ]
    .concat();

    let write_event = match write_stuff_to_gpu(opencl, &stuff, None) {
        Ok(ev) => ev,
        Err(e) => {
            eprintln!("[OpenCL] stuff upload failed: {}", e);
            most.gpu_batch_ok = false;
            return most;
        }
    };

    let kernel_event = match enqueue_diamond_kernel(
        opencl,
        nonce_start,
        repeat,
        unit_size,
        num_work_groups,
        local_work_size,
        Some(&write_event),
    ) {
        Ok(ev) => ev,
        Err(e) => {
            eprintln!("[OpenCL] diamond kernel failed: {}", e.display());
            most.gpu_batch_ok = false;
            return most;
        }
    };

    let mut hashes = vec![0u8; opencl.buffer_best_hashes.len()];
    let mut nonces = vec![0u64; opencl.buffer_best_nonces_diamond.len()];
    if read_diamond_gpu_results(opencl, &kernel_event, &mut hashes, &mut nonces).is_err() {
        most.gpu_batch_ok = false;
        return most;
    }

    for i in 0..num_work_groups as usize {
        let hash_bytes = &hashes[i * 32..(i * 32) + 32].try_into().unwrap();
        let dia_str = diamond_hash(hash_bytes);
        let nonce_bytes = nonces[i].to_be_bytes();
        // Re-hash with the SAME custom-message bytes the GPU kernel was fed:
        // `custom_nonce` is gated by number (empty at or below the custom-message
        // threshold, matching node consensus). Using the raw `custom_message` here
        // would make the verification hash disagree with the GPU result and reject
        // otherwise-valid low-number diamonds.
        let stuff = [
            prevblockhash.as_slice(),
            nonce_bytes.as_slice(),
            address.as_slice(),
            custom_nonce,
        ]
        .concat();
        let ssshash: [u8; 32] = calculate_hash(stuff);

        if let Some(dia_name) = check_diamer_success(number, ssshash, *hash_bytes, dia_str) {
            let name = DiamondName::from(dia_name);
            let number = DiamondNumber::from(number);
            let mut diamint = DiamondMint::with(name, number);
            diamint.d.prev_hash = prevblockhash.clone();
            diamint.d.nonce = Fixed8::from(nonces[i].to_be_bytes());
            diamint.d.address = rwdaddr.clone();
            diamint.d.custom_message = custom_message.clone();
            most.dia_str = dia_str;
            most.u64_nonce = nonces[i];
            most.is_success = Some(diamint);
            most.gpu_batch_ok = true;
            return most;
        } else if diamond_more_power(&dia_str, &most.dia_str) {
            most.dia_str = dia_str;
            most.u64_nonce = nonces[i];
        }
    }

    if opencl.needs_queue_finish {
        if let Err(e) = opencl.queue.finish() {
            eprintln!("[OpenCL] diamond queue finish: {}", e);
            most.gpu_batch_ok = false;
            return most;
        }
    }

    most.gpu_batch_ok = true;
    most
}
