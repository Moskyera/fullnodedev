//! Shared delay macros for poworker / diaworker mining loops.

#[macro_export]
macro_rules! delay_continue_ms {
    ($ms: expr) => {
        std::thread::sleep(std::time::Duration::from_millis($ms));
        continue
    };
}

#[macro_export]
macro_rules! delay_continue {
    ($sec: expr) => {
        std::thread::sleep(std::time::Duration::from_secs($sec));
        continue
    };
}

#[macro_export]
macro_rules! delay_return_ms {
    ($ms: expr) => {
        std::thread::sleep(std::time::Duration::from_millis($ms));
        return
    };
}

#[macro_export]
macro_rules! delay_return {
    ($sec: expr) => {
        std::thread::sleep(std::time::Duration::from_secs($sec));
        return
    };
}
