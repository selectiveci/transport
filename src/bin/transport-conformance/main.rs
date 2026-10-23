#![warn(clippy::perf, clippy::style, clippy::pedantic)]

mod conformance;
mod fixtures;
mod reference_client;

use std::path::PathBuf;

use conformance::{run_suite_with, scenario_names, ConformanceRun, SuiteVia};
use fixtures::FixtureIds;
use reference_client::{RunOpts, Via};
use transport::Error;

const USAGE: &str = "\
usage:
  transport-conformance list
  transport-conformance run [--scenario NAME] [--via direct|stdio|both]
                            [--client COMMAND] [--fixture-ids FILE]
                            [--transport PATH]
  transport-conformance client [--via-stdio]

The transport binary is --transport, else SELECTIVE_TRANSPORT, else the
transport next to this binary. A --client command gets it as
SELECTIVE_TRANSPORT.
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
        Some("list") => {
            for name in scenario_names()? {
                println!("{name}");
            }
            Ok(0)
        }
        Some("run") => run_main(&args[1..]),
        Some("client") => client_main(args[1..].iter().any(|arg| arg == "--via-stdio")),
        Some("--help" | "-h") => {
            eprintln!("{USAGE}");
            Ok(0)
        }
        _ => Err(Error::new(USAGE)),
    }
}

fn run_main(args: &[String]) -> Result<i32, Error> {
    let mut scenario = None;
    let mut via = SuiteVia::Both;
    let mut client = env_value("SELECTIVE_CONFORMANCE_CLIENT");
    let mut fixture_file = env_value("SELECTIVE_CONFORMANCE_FIXTURE_IDS");
    let mut transport = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = || required(args, index + 1, flag);
        match flag {
            "--scenario" => scenario = Some(value()?.to_owned()),
            "--via" => {
                via = match value()? {
                    "direct" => SuiteVia::Direct,
                    "stdio" => SuiteVia::Stdio,
                    "both" => SuiteVia::Both,
                    other => return Err(Error::new(format!("unknown --via {other}"))),
                }
            }
            "--client" => client = Some(value()?.to_owned()),
            "--fixture-ids" => fixture_file = Some(value()?.to_owned()),
            "--transport" => transport = Some(PathBuf::from(value()?)),
            other => return Err(Error::new(format!("unknown run argument {other}\n{USAGE}"))),
        }
        index += 2;
    }
    let fixture_path = fixture_file.map(PathBuf::from);
    let fixtures = match &fixture_path {
        Some(path) => FixtureIds::load(path)?,
        None => FixtureIds::default(),
    };
    let transport = transport.or_else(default_transport);
    let run = ConformanceRun {
        filter: scenario.as_deref(),
        via,
        client: client.as_deref(),
        fixtures,
        fixture_file: fixture_path.as_deref(),
        transport: transport.as_deref(),
    };
    block_on(run_suite_with(&std::env::current_exe()?, run))?;
    Ok(0)
}

fn client_main(via_stdio: bool) -> Result<i32, Error> {
    let url = env_value("SELECTIVE_TRANSPORT_URL")
        .ok_or_else(|| Error::new("client requires SELECTIVE_TRANSPORT_URL"))?;
    Ok(block_on(reference_client::run(RunOpts {
        url,
        via: if via_stdio { Via::Stdio } else { Via::Direct },
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

fn required<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, Error> {
    match args.get(index).filter(|value| !value.is_empty()) {
        Some(value) => Ok(value.as_str()),
        None => Err(Error::new(format!("missing value for {flag}"))),
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}
