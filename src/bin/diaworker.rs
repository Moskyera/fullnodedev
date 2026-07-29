use app::*;

fn main() {
    // Same as poworker: the panel cannot read this process's console on Windows,
    // so the worker keeps a rolling copy of its output beside its config.
    let config = sys::resolve_config_path("./diaworker.config.ini");
    match worker_log::init_beside_config(&config, "diaworker") {
        Ok(path) => wlogln!("[log] worker log: {}", path.display()),
        Err(error) => eprintln!("[log] no worker log ({error}); console only."),
    }
    wlogln!(
        "[Version] Diamond miner worker v{}, build time: {}.",
        HACASH_NODE_VERSION,
        HACASH_NODE_BUILD_TIME
    );
    app::diaworker::diaworker()
}
