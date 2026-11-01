#![warn(clippy::perf, clippy::style, clippy::pedantic)]

mod fixtures;
mod reference_client;

use std::path::PathBuf;

use reference_client::{RunOpts, Via};
use transport::Error;

const USAGE: &str = "\
usage:
  transport-conformance client [--via-stdio | --via-fifo]
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

fn dispatch() -> Result<i32, Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("client") => client_main(&args[1..]),
        Some("--help" | "-h") => {
            eprintln!("{USAGE}");
            Ok(0)
        }
        _ => Err(Error::new(USAGE)),
    }
}

fn client_main(args: &[String]) -> Result<i32, Error> {
    let url = env_value("SELECTIVE_TRANSPORT_URL")
        .ok_or_else(|| Error::new("client requires SELECTIVE_TRANSPORT_URL"))?;
    let via = if args.iter().any(|arg| arg == "--via-fifo") {
        Via::Fifo
    } else if args.iter().any(|arg| arg == "--via-stdio") {
        Via::Stdio
    } else {
        Via::Direct
    };
    Ok(block_on(reference_client::run(RunOpts {
        url,
        via,
        transport: default_transport(),
        catch_signals: true,
    })))
}

fn default_transport() -> Option<PathBuf> {
    env_value("SELECTIVE_TRANSPORT")
        .map(PathBuf::from)
        .or_else(|| {
            let sibling = std::env::current_exe()
                .ok()?
                .with_file_name(format!("transport{}", std::env::consts::EXE_SUFFIX));
            sibling.is_file().then_some(sibling)
        })
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}
