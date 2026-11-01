#![warn(clippy::perf, clippy::style, clippy::pedantic)]

mod engine;

use std::path::PathBuf;

const USAGE: &str = "\
usage:
  transport <websocket-url> <runner-id>
  transport --snapshot [dir]
  transport --build-env
";

fn main() {
    let code = match dispatch() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            1
        }
    };
    std::process::exit(code);
}

fn dispatch() -> Result<i32, transport::Error> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => Err(transport::Error::new(USAGE)),
        Some("--help" | "-h") => {
            eprintln!("{USAGE}");
            Ok(0)
        }
        Some("--snapshot") => snapshot_main(args.next()),
        Some("--build-env") => {
            println!("{}", serde_json::to_string(&transport::detect_build_env())?);
            Ok(0)
        }
        Some(url) => match args.next() {
            Some(runner_id) => Ok(block_on(engine::run_fifo(url, &runner_id))),
            None => Err(transport::Error::new(USAGE)),
        },
    }
}

fn snapshot_main(dir: Option<String>) -> Result<i32, transport::Error> {
    let path = dir.map_or_else(|| PathBuf::from("."), PathBuf::from);
    match transport::snapshot_tree(&path) {
        Some(tree) => {
            println!("{}", serde_json::to_string(&tree)?);
            Ok(0)
        }
        None => Err(transport::Error::new("not a git checkout")),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    runtime().block_on(future)
}
