#![warn(clippy::perf, clippy::style, clippy::pedantic)]

mod engine;

use std::process::exit;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <url> <runner_id>", args[0]);
        exit(1);
    }

    exit(engine::run_fifo(&args[1], &args[2]).await);
}
