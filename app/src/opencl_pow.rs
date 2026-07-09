fn do_group_block_mining_opencl(
    opencl: &OpenCLResources,
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    num_work_groups: u32,
    local_work_size: u32,
    unit_size: u32,
) -> std::result::Result<(u32, [u8; 32]), String> {
    let mut most_nonce = 0u32;
    let mut most_hash = [255u8; 32];
    let repeat = x16rs::block_hash_repeat(height) as u32;

    let write_event = write_stuff_to_gpu(opencl, &block_intro, None)?;

    let kernel_event = enqueue_mining_kernel(
        opencl,
        nonce_start,
        repeat,
        unit_size,
        num_work_groups,
        local_work_size,
        Some(&write_event),
    )?;

    let mut hashes = vec![0u8; opencl.buffer_best_hashes.len()];
    let mut nonces = vec![0u32; opencl.buffer_best_nonces.len()];
    read_block_gpu_results(opencl, &kernel_event, &mut hashes, &mut nonces)?;

    for i in 0..num_work_groups as usize {
        let hash_bytes = &hashes[i * 32..(i * 32) + 32];
        if hash_more_power(hash_bytes, &most_hash) {
            most_hash.copy_from_slice(hash_bytes);
            most_nonce = nonces[i];
        }
    }

    Ok((most_nonce, most_hash))
}