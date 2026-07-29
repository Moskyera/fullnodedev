use app::*;

fn main() {
    // Open the rolling log before anything is printed, so the banner is in it
    // too. On Windows the panel gives this worker its own console window and
    // therefore cannot read a single line of its output; this file is the only
    // copy the panel can see, and it is what the event feed is built from.
    //
    // The config path is resolved exactly as `poworker()` will resolve it, so
    // the log always lands beside the config the worker actually read.
    let config = sys::resolve_config_path("./poworker.config.ini");
    match worker_log::init_beside_config(&config, "poworker") {
        Ok(path) => wlogln!("[log] worker log: {}", path.display()),
        // Not fatal, and never allowed to be: a worker that cannot write a log
        // can still mine.
        Err(error) => eprintln!("[log] no worker log ({error}); console only."),
    }
    wlogln!(
        "[Version] HAC miner worker v{}, build time: {}.",
        HACASH_NODE_VERSION,
        HACASH_NODE_BUILD_TIME
    );
    app::poworker::poworker()
}
