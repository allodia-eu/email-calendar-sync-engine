//! The `engine-cli` binary: a thin shim over [`engine_cli::run`].
//!
//! All parsing, dispatch, and output live in the library (and are tested there);
//! this prints the result or the error.
//!
//! ```text
//! engine-cli ingest   --db <path> --account <id> [--zone <iana>] [--horizon-start <YYYY-MM-DD>] [--horizon-end <YYYY-MM-DD>] <fixture.json>
//! engine-cli reexpand --db <path> --account <id> [--zone <iana>] [--horizon-start <YYYY-MM-DD>] [--horizon-end <YYYY-MM-DD>]
//! engine-cli search   --db <path> --account <id> --kind <mail|calendar> [--limit <n>] <query...>
//! ```

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match engine_cli::run(&args).await {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
