//! Panic firewall shared by the block (HAC) and diamond (HACD) mining threads.
//!
//! Both miners have exactly one thread that drains results and submits winners.
//! A panic on that thread would end every payout while the process still looked
//! perfectly healthy, so each loop iteration runs inside `catch_unwind` here.

pub(crate) fn panic_reason(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        text
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.as_str()
    } else {
        "unknown panic payload"
    }
}

/// Panic firewall for the long-lived mining threads. The result thread is the
/// only path that submits winning work, so a panic that ended it would stop
/// every payout while the process still looked perfectly healthy. Contain the
/// panic, log it, and let the loop keep running.
pub(crate) fn guard_mining_iteration(label: &str, body: impl FnOnce()) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        eprintln!(
            "[Mining] {} panicked and was contained: {}",
            label,
            panic_reason(&*payload)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_iteration_is_contained_and_reported() {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut iterations = 0u32;
        for round in 0..3 {
            guard_mining_iteration("test loop", || {
                if round == 1 {
                    panic!("simulated result thread panic");
                }
            });
            iterations += 1;
        }
        std::panic::set_hook(previous_hook);
        assert_eq!(iterations, 3);
    }

    #[test]
    fn a_panic_payload_of_any_shape_yields_a_reason() {
        assert_eq!(panic_reason(&"static text"), "static text");
        assert_eq!(panic_reason(&"owned text".to_string()), "owned text");
        assert_eq!(panic_reason(&7u32), "unknown panic payload");
    }
}
