//! OpenCL diamond mining kernel path (submodule of diaworker).

use field::{Address, DiamondName, DiamondNumber, Fixed8, Hash};
use mint::action::DIAMOND_ABOVE_NUMBER_OF_CREATE_BY_CUSTOM_MESSAGE;
use mint::action::DiamondMint;
use x16rs::calculate_hash;
use x16rs::diamond_hash;
use x16rs::x16rs_hash;

use crate::hash_util::diamond_better;
use crate::opencl_gpu::{
    OpenCLResources, enqueue_diamond_kernel, read_diamond_gpu_results, write_stuff_to_gpu,
};

use super::{DIAMOND_HASH_LEN, DiamondMiningResult, check_diamer_success, diamond_pre_image};

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
    // The kernel overwrites the 8 nonce bytes per work item; upload with a zero
    // nonce. Same builder the CPU recompute below uses, so the two cannot drift.
    let stuff = diamond_pre_image(prevblockhash, &[0u8; 8], rwdaddr, custom_nonce);
    let stuff_len = stuff.len() as u32;
    // sha3_256_hash_diamond has exactly two padding layouts: 61 bytes without a
    // custom message and 93 bytes with one. Any other length would be hashed with
    // the wrong pad and silently produce a hash that is not the consensus one, so
    // refuse the batch instead of mining garbage.
    debug_assert!(stuff_len == 61 || stuff_len == 93);
    if stuff_len != 61 && stuff_len != 93 {
        wlogerr!(
            "[OpenCL] diamond pre-image length {} is neither 61 nor 93; skipping batch",
            stuff_len
        );
        most.gpu_batch_ok = false;
        return most;
    }

    let write_event = match write_stuff_to_gpu(opencl, &stuff, None) {
        Ok(ev) => ev,
        Err(e) => {
            wlogerr!("[OpenCL] stuff upload failed: {}", e);
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
        stuff_len,
        Some(&write_event),
    ) {
        Ok(ev) => ev,
        Err(e) => {
            wlogerr!("[OpenCL] diamond kernel failed: {}", e.display());
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

    let mut found_success = false;
    // How many of the returned work-group bests actually reproduce on the CPU. A
    // batch where NOTHING reproduces is a broken device, not a healthy empty
    // batch, and must not be reported to the health tracker as a success.
    let mut verified_count = 0usize;
    for i in 0..num_work_groups as usize {
        // Real bounds guards. The previous `hashes[i * 32..]` + try_into pair read
        // like one, but an out-of-range index panics inside the slice expression
        // before try_into is ever reached, so its Err arm could never fire.
        let Some(chunk) = hashes.get(i * 32..(i * 32) + 32) else {
            continue;
        };
        let Ok(hash_bytes) = <[u8; 32]>::try_from(chunk) else {
            continue;
        };
        let Some(&nonce) = nonces.get(i) else {
            continue;
        };
        // Reject nonces outside the batch window (corrupt GPU read).
        if nonce.wrapping_sub(nonce_start) >= nonce_space {
            continue;
        }
        let nonce_bytes = nonce.to_be_bytes();
        // Re-hash with the SAME custom-message bytes the GPU kernel was fed:
        // `custom_nonce` is gated by number (empty at or below the custom-message
        // threshold, matching node consensus).
        let stuff = diamond_pre_image(prevblockhash, &nonce_bytes, rwdaddr, custom_nonce);
        let ssshash: [u8; 32] = calculate_hash(stuff);
        // Full medium-hash recompute (parity with block verify_gpu_best_result).
        let expected_medium = x16rs_hash(repeat as i32, &ssshash);
        if expected_medium != hash_bytes {
            continue;
        }
        verified_count += 1;
        let dia_str = diamond_hash(&hash_bytes);

        if let Some(dia_name) = check_diamer_success(number, ssshash, hash_bytes, dia_str) {
            let name = DiamondName::from(dia_name);
            let number = DiamondNumber::from(number);
            let mut diamint = DiamondMint::with(name, number);
            diamint.d.prev_hash = prevblockhash.clone();
            diamint.d.nonce = Fixed8::from(nonce_bytes);
            diamint.d.address = rwdaddr.clone();
            diamint.d.custom_message = custom_message.clone();
            most.dia_str = dia_str;
            most.u64_nonce = nonce;
            most.is_success = Some(diamint);
            most.gpu_batch_ok = true;
            found_success = true;
            break;
        } else if diamond_better(&dia_str, &most.dia_str) {
            most.dia_str = dia_str;
            most.u64_nonce = nonce;
        }
    }

    // Always finish the queue when required, including the early success path,
    // so AMD RDNA/duplicate ICD does not leave work outstanding.
    if opencl.needs_queue_finish {
        if let Err(e) = opencl.queue.finish() {
            wlogerr!("[OpenCL] diamond queue finish: {}", e);
            // Report the driver failure, but NEVER discard `most.is_success`: that
            // DiamondMint was already verified on the CPU above (x16rs_hash
            // recompute, then check_diamond_hash_result + check_diamond_difficulty
            // via check_diamer_success). Its validity is a pure function of
            // prev_hash/nonce/address/custom_message and does not depend on the
            // command queue, so dropping it here would be pure loss of mined value.
            most.gpu_batch_ok = false;
            return most;
        }
    }

    if !found_success {
        most.gpu_batch_ok = verified_count > 0 || num_work_groups == 0;
    }
    most
}
