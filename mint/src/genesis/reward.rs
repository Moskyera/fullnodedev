
const BLOCK_REWARD_SEC_TWO: usize = 66;
const BLOCK_REWARD_STEP_BLOCK: u64 = 10_0000;
const BLOCK_REWARD_DEF_LIST: [u8; BLOCK_REWARD_SEC_TWO] = [
    1, 1, 2, 3, 5, 8,
    8,8,8,8,8,8,8,8,8,8,
    5,5,5,5,5,5,5,5,5,5,
    3,3,3,3,3,3,3,3,3,3,
    2,2,2,2,2,2,2,2,2,2,
    1,1,1,1,1,1,1,1,1,1,
    1,1,1,1,1,1,1,1,1,1,
];

/*
* Currency release algorithm: 22 million in the first 66 years
*/
pub fn block_reward_number(block_height: u64) -> u8 {
    let stp = BLOCK_REWARD_STEP_BLOCK;
    let lis = BLOCK_REWARD_DEF_LIST;
    let sct = BLOCK_REWARD_SEC_TWO as u64;
    let curstp = block_height / stp;
    if curstp >= sct {
        return 1 // after 66 years
    }
    // before 66 years
    lis[curstp as usize]
}

pub fn block_reward(block_height: u64) -> Amount {
	let num = block_reward_number(block_height);
	return Amount::small_mei(num)
}

/*
* Total block reward issued from height 0 up to and including block_height.
* Reporting only (the /supply api), no consensus rule reads this.
*/
pub fn cumulative_block_reward(block_height: u64) -> u64 {
    let stp = BLOCK_REWARD_STEP_BLOCK;
    let mut cbhei = block_height + 1;
    let mut ttcoin = 0u64;
    let mut in_def_list = false;
    for (_, v) in BLOCK_REWARD_DEF_LIST.iter().enumerate() {
        let v = *v as u64;
        if cbhei < stp {
            ttcoin += cbhei * v;
            in_def_list = true;
            break // finish
        }
        ttcoin += stp * v;
        cbhei -= stp; // next
    }
    if !in_def_list {
        // the definition list only covers the first 66 years, past it block_reward_number()
        // returns a perpetual 1 per block and the remainder must be counted too
        ttcoin += cbhei;
    }
    // the genesis block coinbase (1 HAC) is never applied to state by do_initialize, so it
    // is not part of the issued supply
    ttcoin - 1
}


#[cfg(test)]
mod block_reward_tests {
    use super::*;

    // 66 segments of 100_000 blocks, 22 million HAC in the first 66 years
    const FIRST_66_YEARS: u64 = 2200_0000;
    const LAST_DEF_HEIGHT: u64 = BLOCK_REWARD_STEP_BLOCK * BLOCK_REWARD_SEC_TWO as u64; // 6_600_000

    #[test]
    fn definition_list_issues_22_million_in_the_first_66_years() {
        let sum: u64 = BLOCK_REWARD_DEF_LIST
            .iter()
            .map(|v| *v as u64 * BLOCK_REWARD_STEP_BLOCK)
            .sum();
        assert_eq!(sum, FIRST_66_YEARS);
        // heights 0 .. 6_599_999 are exactly the definition list, minus the genesis coinbase
        assert_eq!(cumulative_block_reward(LAST_DEF_HEIGHT - 1), FIRST_66_YEARS - 1);
    }

    #[test]
    fn perpetual_tail_after_66_years_is_counted() {
        assert_eq!(block_reward_number(LAST_DEF_HEIGHT), 1);
        // block 6_600_000 itself pays the perpetual 1 HAC
        assert_eq!(cumulative_block_reward(LAST_DEF_HEIGHT), FIRST_66_YEARS);
        // and every further block adds another 1 HAC
        assert_eq!(
            cumulative_block_reward(LAST_DEF_HEIGHT + 999),
            FIRST_66_YEARS + 999
        );
    }

    #[test]
    fn cumulative_reward_inside_the_definition_list_is_unchanged() {
        assert_eq!(cumulative_block_reward(0), 0);
        assert_eq!(cumulative_block_reward(80_0000), 360_0007);
    }
}


/////////////////////



 /*
 pub fn block_reward_number(block_height: u64) -> u8 {
    let part1 = [1u8, 1, 2, 3, 5, 8];
    let part2 = [8u8, 5, 3, 2, 1, 1];
    let part3 = 1u8;
    let tbn1: u64 =  10_0000;
    let tbn2: u64 = 100_0000;
    let spx1: u64 = part1.len() as u64 * tbn1;
    let spx2: u64 = part2.len() as u64 * tbn2 + spx1;
    let mut basenum = block_height;
    if block_height <= spx1 {
        return part1[(basenum/tbn1) as usize]
    }
    if block_height <= spx2 {
        basenum -= spx1;
        return part2[(basenum/tbn2) as usize]
    }
    return part3
}
*/
