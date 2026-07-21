use x16rs::*;

#[test]
fn t_shas() {
    // sha2
    let cres = hex::encode(sha2("123456"));
    assert_eq!(
        cres,
        "8d969eef6ecad3c29a3a629280e686cf0c3f5d5a86aff3ca12020c923adc6c92"
    );

    // sha3
    let cres = hex::encode(sha3("123456"));
    assert_eq!(
        cres,
        "d7190eb194ff9494625514b6d178c87f99c5973e28c398969d2233f2960a573e"
    );

    /*
    genesis block head meta:
    01
    0000000000
    005c57b08c
    0000000000000000000000000000000000000000000000000000000000000000
    ad557702fc70afaf70a855e7b8a4400159643cb5a7fc8a89ba2bce6f818a9b01
    00000001
    098b3445
    00000000
    0000
    */
    // block_hash
    let hdts = hex::decode("010000000000005c57b08c0000000000000000000000000000000000000000000000000000000000000000ad557702fc70afaf70a855e7b8a4400159643cb5a7fc8a89ba2bce6f818a9b0100000001098b3445000000000000").unwrap();
    let cres = hex::encode(block_hash(1, &hdts));
    assert_eq!(
        cres,
        "000000077790ba2fcdeaef4a4299d9b667135bac577ce204dee8388f1b97f7e6"
    );
}

#[test]
fn x16rs_ffi_output_and_protocol_vectors_are_stable() {
    let zero = [0u8; 32];
    assert_eq!(
        hex::encode(x16rs_hash(1, &zero)),
        "6fe2a4b96f71518b7603e5c63702588ba816885aa1ce5908de31335e11473460"
    );

    let mut sequential = [0u8; 32];
    for (index, byte) in sequential.iter_mut().enumerate() {
        *byte = index as u8;
    }
    assert_eq!(
        hex::encode(x16rs_hash(1, &sequential)),
        "5f4b9c2bc542352be3bd684ce2228447ba14b3cf32a41b04d18b52290435cea5"
    );

    // The protocol selects the algorithm from the low nibble of little-endian
    // word 7 (byte 28 here), not from the final byte of the digest. This vector
    // therefore exercises algorithm 2 (Groestl) and guards CPU/GPU validation.
    let groestl_input: [u8; 32] =
        hex::decode("73710d4acc7ace564b0239839f88c735ad499a667a197974634a52292282fa04")
            .unwrap()
            .try_into()
            .unwrap();
    assert_eq!(groestl_input[28] & 0x0f, 2);
    assert_eq!(
        hex::encode(x16rs_hash(1, &groestl_input)),
        "d4f2ebda478be732d5e6efe5b4c6588c7057a781c3bbd8a610fb3534210b6a7f"
    );
}
