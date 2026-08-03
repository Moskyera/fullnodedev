//! How many CPU threads a worker takes, decided from the machine it is running
//! on rather than from a number somebody typed into a shipped config file.
//!
//! # What was measured
//!
//! HACD (diamond) hashrate on a Ryzen 9 9950X, 16 physical cores and 32 logical,
//! one binary, one process, nothing changed between rows but the thread count:
//!
//! ```text
//!  threads         H/s    per thread    vs 1 thread
//!        1      71,728        71,728          1.00x
//!        6     320,097        53,350          4.46x
//!       16     853,739        53,359         11.90x
//!       32   1,442,210        45,069         20.11x
//! ```
//!
//! # All logical, not all physical, and why
//!
//! The 16 row is exactly the physical core count and the 32 row adds SMT: +69%
//! for threads that share an execution port with one already running. x16rs is a
//! long chain of small dependent hashes, so the second thread on a core fills
//! stalls the first one leaves rather than fighting it for issue slots. Stopping
//! at the physical count would give away 41% of this machine's diamond rate, so
//! the default counts LOGICAL CPUs.
//!
//! There is also nothing in std that reports physical cores: `available_parallelism`
//! reports logical ones. "All physical" would need a new dependency before it
//! could even be expressed, and the measurement says it would be the worse
//! answer, so this module never tries.
//!
//! # But not every logical thread
//!
//! Per-thread throughput has already fallen 37% by 32 threads, which makes the
//! last two threads on this CPU the cheapest ones on the machine to give back:
//! about 45k H/s out of 1.44M, near 3%. They buy the fullnode the scheduling it
//! needs to serve its own RPC and consensus work, they buy a co-running GPU
//! miner a thread to feed its card from, and they buy the operator a desktop
//! that still redraws. A miner that takes the very last thread is a miner people
//! turn off, which costs 100%.
//!
//! # The two answers are not the same number
//!
//! [`hacd_threads_for`] is for a process where the CPU IS the miner. CPU assist
//! next to a GPU is a different question and gets [`cpu_assist_threads_for`]:
//! there the CPU is helping a device worth several hundred CPU threads, and a
//! stalled GPU feed thread costs far more than the assist threads earn. That
//! number is deliberately a small fraction and is a safety choice, not a
//! measurement: nothing here was measured with a GPU in the same process.

/// Logical threads left to everything that is not the hash loop: one for the
/// fullnode this worker talks to, one for a GPU feed thread or the desktop.
///
/// Two, not one, and not four. See the module docs: on the measured CPU these
/// are worth about 3% of the diamond rate, and both of the things they pay for
/// are things whose absence costs much more than 3%.
pub const HOST_RESERVE_THREADS: u32 = 2;

/// Logical CPUs this machine reports, or 1 when the OS will not say.
///
/// 1 rather than a guessed 8: an unknown machine gets the answer that cannot
/// oversubscribe anything, and every caller below floors at 1 anyway.
pub fn logical_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// HACD thread count for a machine with `logical` logical CPUs: all of them but
/// the host reserve, and never zero.
pub fn hacd_threads_for(logical: u32) -> u32 {
    logical.saturating_sub(HOST_RESERVE_THREADS).max(1)
}

/// HACD thread count for THIS machine.
pub fn hacd_threads() -> u32 {
    hacd_threads_for(logical_cpus())
}

/// CPU-assist thread count next to a GPU, for a machine with `logical` logical
/// CPUs.
///
/// A quarter of the machine, floored at 1 and never more than [`hacd_threads_for`]
/// would take. The quarter is inherited from the panel's old "Automatic CPU
/// assist (safe)" entry, which was already core-derived; what it did not have
/// was a ceiling that scaled, so it stopped at 8 threads no matter how large the
/// CPU was. The ceiling is gone, the fraction stays, because the fraction is the
/// part that was never wrong.
pub fn cpu_assist_threads_for(logical: u32) -> u32 {
    (logical / 4).clamp(1, hacd_threads_for(logical))
}

/// CPU-assist thread count for THIS machine.
pub fn cpu_assist_threads() -> u32 {
    cpu_assist_threads_for(logical_cpus())
}

/// Clamp a configured thread count down to what is safe to run beside a GPU.
///
/// Zero is preserved: for a GPU miner, `supervene = 0` means "no CPU assist at
/// all", which is a real and common choice and must never be clamped UP into
/// spawning threads nobody asked for.
pub fn cap_cpu_assist_for(configured: u32, logical: u32) -> u32 {
    if configured == 0 {
        return 0;
    }
    configured.min(cpu_assist_threads_for(logical))
}

/// Clamp a configured CPU-assist count for THIS machine.
pub fn cap_cpu_assist(configured: u32) -> u32 {
    cap_cpu_assist_for(configured, logical_cpus())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured machine. This is the row the whole module exists for: the
    /// shipped default used to be 6 on it, which is 22% of what it can do.
    #[test]
    fn the_9950x_gets_thirty_of_its_thirty_two_threads() {
        assert_eq!(hacd_threads_for(32), 30);
        // 6 was the shipped number, 8 was the panel's "Automatic". Both are gone.
        assert_ne!(hacd_threads_for(32), 6);
        assert_ne!(hacd_threads_for(32), 8);
        // And it is not the physical core count either: SMT measured +69%.
        assert_ne!(hacd_threads_for(32), 16);
    }

    /// Small and strange machines must still get a runnable answer, and must
    /// never get one larger than the machine.
    #[test]
    fn every_machine_gets_at_least_one_thread_and_never_more_than_it_has() {
        for logical in 0..=256u32 {
            let hacd = hacd_threads_for(logical);
            let assist = cpu_assist_threads_for(logical);
            assert!(hacd >= 1, "logical={logical}");
            assert!(assist >= 1, "logical={logical}");
            if logical >= 1 {
                assert!(hacd <= logical, "logical={logical}");
                assert!(assist <= logical, "logical={logical}");
            }
            assert!(assist <= hacd, "logical={logical}");
        }
    }

    /// A bigger CPU must never be given fewer threads than a smaller one. The
    /// old panel list failed this: it capped "Automatic" at 8 forever.
    #[test]
    fn the_answer_never_shrinks_as_the_machine_grows() {
        for logical in 1..=256u32 {
            assert!(
                hacd_threads_for(logical) >= hacd_threads_for(logical - 1),
                "logical={logical}"
            );
            assert!(
                cpu_assist_threads_for(logical) >= cpu_assist_threads_for(logical - 1),
                "logical={logical}"
            );
        }
        // The specific failure that is being fixed: the old rule was
        // (logical / 4).clamp(2, 8), which returns 8 for 32 and for 128 alike.
        assert!(cpu_assist_threads_for(128) > cpu_assist_threads_for(32));
    }

    /// The reserve is the whole point of the HACD number, so it is asserted
    /// rather than left to the arithmetic.
    #[test]
    fn the_host_reserve_is_left_free_on_any_machine_big_enough_to_spare_it() {
        for logical in (HOST_RESERVE_THREADS + 1)..=256u32 {
            assert_eq!(
                logical - hacd_threads_for(logical),
                HOST_RESERVE_THREADS,
                "logical={logical}"
            );
        }
    }

    /// HACD and CPU assist answer different questions, and the assist answer has
    /// to be the smaller one on any machine where "smaller" exists. If these
    /// ever collapse into one number, a GPU rig starts running the diamond
    /// miner's thread count next to its card.
    #[test]
    fn cpu_assist_is_a_fraction_of_the_hacd_count_on_a_real_cpu() {
        for logical in [8u32, 12, 16, 24, 32, 64, 128] {
            assert!(
                cpu_assist_threads_for(logical) * 2 <= hacd_threads_for(logical),
                "logical={logical}"
            );
        }
        assert_eq!(cpu_assist_threads_for(32), 8);
        assert_eq!(cpu_assist_threads_for(16), 4);
    }

    /// The reserve exists to be spent on exactly two things, so this is the
    /// arithmetic it has to survive: HACD taking its default while the GPU block
    /// miner runs on the same box with one thread per card to feed it, and the
    /// fullnode both of them talk to running too.
    ///
    /// It holds for one GPU, which is what the reserve is sized for. A rig with
    /// several cards, or one that also turns on poworker's `cpu_assist`, is
    /// spending the same two threads more than once; that is why the diamond
    /// worker prints what it took and what it left.
    #[test]
    fn the_hacd_default_leaves_room_for_a_gpu_feed_thread_and_the_node() {
        for logical in 4..=256u32 {
            let hacd = hacd_threads_for(logical);
            let one_gpu_feed = 1;
            let fullnode = 1;
            assert!(
                hacd + one_gpu_feed + fullnode <= logical,
                "logical={logical}: HACD takes {hacd} and leaves no room to co-run"
            );
        }
        // The shipped GPU-first configs default `cpu_assist = false`, so a
        // co-running poworker costs exactly the feed thread this reserves.
        assert_eq!(hacd_threads_for(32) + 1 + 1, 32);
    }

    /// "No CPU assist" is a choice, not a missing value.
    #[test]
    fn capping_cpu_assist_never_invents_threads() {
        for logical in 1..=64u32 {
            assert_eq!(cap_cpu_assist_for(0, logical), 0, "logical={logical}");
            for configured in 1..=64u32 {
                let capped = cap_cpu_assist_for(configured, logical);
                assert!(capped >= 1);
                assert!(capped <= configured);
                assert!(capped <= cpu_assist_threads_for(logical));
            }
        }
        // The case this exists for: an operator picks the full HACD ladder entry
        // while a GPU is selected. 30 assist threads beside a card is not what
        // the picker meant.
        assert_eq!(cap_cpu_assist_for(30, 32), 8);
    }

    /// This machine, whatever it is, must produce something usable. Catches a
    /// platform where `available_parallelism` fails and the fallback is wrong.
    #[test]
    fn this_machine_gets_a_usable_answer() {
        assert!(logical_cpus() >= 1);
        assert!(hacd_threads() >= 1);
        assert!(cpu_assist_threads() >= 1);
        assert!(cpu_assist_threads() <= hacd_threads());
    }
}
