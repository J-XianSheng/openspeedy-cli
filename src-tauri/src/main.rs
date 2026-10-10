//! OpenSpeedy command-line entry point.
//!
//! This binary carries no GUI and no WebView dependency: process listing,
//! injection and speed control are all driven from command-line arguments.
//! Run `openspeedy help` for usage.

fn main() {
    // First statement in the process: panics before this point are the only
    // ones that cannot be logged.
    openspeedy_lib::applog::init();
    openspeedy_lib::applog::info(
        "startup",
        format!(
            "openspeedy CLI {:?} invoked",
            std::env::args().collect::<Vec<_>>()
        ),
    );

    let code = openspeedy_lib::cli::run(std::env::args().skip(1).collect());

    openspeedy_lib::applog::info(
        "shutdown",
        format!("command finished with exit code {code}"),
    );
    std::process::exit(code);
}
