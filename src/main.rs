#![warn(clippy::perf, clippy::style, clippy::pedantic)]

mod engine;

use std::path::PathBuf;

use transport::conformance::{run_suite_with, scenario_names, ConformanceRun, SuiteVia};
use transport::reference_client::{RunOpts, Via};
use transport::upload::{
    resolve_run_identity, split_fragment_stdin, upload_fragment, FragmentUpload,
};
use transport::FixtureIds;

const USAGE: &str = "\
usage:
  transport <websocket-url> <runner-id>
  transport --stdio
  transport --snapshot [dir]
  transport --upload-fragment [--run-id ID] [--run-attempt N] [--runner-id ID]
  transport --build-env
  transport conformance list
  transport conformance run [--scenario NAME] [--via direct|stdio|both]
                            [--client COMMAND] [--fixture-ids FILE]
  transport conformance-client [--via-stdio]
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
        Some("--stdio") => Ok(block_on(
            engine::run_stdio(transport::Keepalive::from_env()),
        )),
        Some("--snapshot") => snapshot_main(args.next()),
        Some("--upload-fragment") => upload_main(&args.collect::<Vec<_>>()),
        Some("--build-env") => {
            println!("{}", serde_json::to_string(&transport::detect_build_env())?);
            Ok(0)
        }
        Some("conformance") => conformance_main(&args.collect::<Vec<_>>()),
        Some("conformance-client") => conformance_client_main(args.any(|arg| arg == "--via-stdio")),
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

fn upload_main(args: &[String]) -> Result<i32, transport::Error> {
    let mut flag_run_id = String::new();
    let mut flag_run_attempt = String::new();
    let mut flag_runner_id = String::new();
    let mut index = 0;
    while index < args.len() {
        let value = required(args, index + 1, &args[index])?.to_owned();
        match args[index].as_str() {
            "--run-id" => flag_run_id = value,
            "--run-attempt" => flag_run_attempt = value,
            "--runner-id" => flag_runner_id = value,
            other => {
                return Err(transport::Error::new(format!(
                    "unknown --upload-fragment argument {other}\n{USAGE}"
                )));
            }
        }
        index += 2;
    }
    let raw = read_stdin()?;
    let (payload, gzip_body) = split_fragment_stdin(&raw)?;
    if gzip_body.is_empty() {
        return Err(transport::Error::new(
            "--upload-fragment expected a gzip body on stdin",
        ));
    }
    let detected = transport::detect_build_env();
    let (run_id, run_attempt, runner_id) = resolve_run_identity(
        &flag_run_id,
        &flag_run_attempt,
        &flag_runner_id,
        payload.as_ref(),
        &detected,
    );
    let upload = FragmentUpload {
        host: detected.host,
        api_key: env_required("SELECTIVE_TEST_MAP_UPLOAD_KEY")?,
        run_id,
        run_attempt,
        runner_id,
        gzip_body,
    };
    block_on(upload_fragment(&upload)).map_err(|err| {
        transport::Error::new(format!(
            "Selective could not upload this runner's test map ({err})."
        ))
    })?;
    Ok(0)
}

fn conformance_main(args: &[String]) -> Result<i32, transport::Error> {
    let mut scenario = None;
    let mut via = SuiteVia::Both;
    let mut command = None;
    let mut client = std::env::var("SELECTIVE_CONFORMANCE_CLIENT")
        .ok()
        .filter(|value| !value.is_empty());
    let mut fixture_file = std::env::var("SELECTIVE_CONFORMANCE_FIXTURE_IDS")
        .ok()
        .filter(|value| !value.is_empty());
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "list" => command = Some("list"),
            "run" => command = Some("run"),
            "--scenario" => {
                index += 1;
                scenario = Some(required(args, index, "--scenario")?.to_owned());
            }
            "--via" => {
                index += 1;
                via = match required(args, index, "--via")? {
                    "direct" => SuiteVia::Direct,
                    "stdio" => SuiteVia::Stdio,
                    "both" => SuiteVia::Both,
                    other => {
                        return Err(transport::Error::new(format!("unknown --via {other}")));
                    }
                };
            }
            "--client" => {
                index += 1;
                client = Some(required(args, index, "--client")?.to_owned());
            }
            "--fixture-ids" => {
                index += 1;
                fixture_file = Some(required(args, index, "--fixture-ids")?.to_owned());
            }
            other => {
                return Err(transport::Error::new(format!(
                    "unknown conformance argument {other}\n{USAGE}"
                )));
            }
        }
        index += 1;
    }
    match command {
        Some("list") => {
            for name in scenario_names()? {
                println!("{name}");
            }
            Ok(0)
        }
        Some("run") => {
            let bin = std::env::current_exe()?;
            let fixture_path = fixture_file.as_deref().map(PathBuf::from);
            let fixtures = match fixture_path.as_deref() {
                Some(path) => FixtureIds::load(path)?,
                None => FixtureIds::default(),
            };
            let run = ConformanceRun {
                filter: scenario.as_deref(),
                via,
                client: client.as_deref(),
                fixtures,
                fixture_file: fixture_path.as_deref(),
            };
            block_on(run_suite_with(&bin, run))?;
            Ok(0)
        }
        _ => Err(transport::Error::new(USAGE)),
    }
}

fn conformance_client_main(via_stdio: bool) -> Result<i32, transport::Error> {
    let url = std::env::var("SELECTIVE_TRANSPORT_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            transport::Error::new("conformance-client requires SELECTIVE_TRANSPORT_URL")
        })?;
    Ok(block_on(transport::reference_client::run(RunOpts {
        url,
        via: if via_stdio { Via::Stdio } else { Via::Direct },
        catch_signals: true,
    })))
}

fn required<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str, transport::Error> {
    match args.get(index).filter(|value| !value.is_empty()) {
        Some(value) => Ok(value.as_str()),
        None => Err(transport::Error::new(format!("missing value for {flag}"))),
    }
}

fn env_required(key: &str) -> Result<String, transport::Error> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(transport::Error::new(format!("missing {key}"))),
    }
}

fn read_stdin() -> Result<Vec<u8>, transport::Error> {
    use std::io::Read;
    let mut body = Vec::new();
    std::io::stdin().read_to_end(&mut body)?;
    Ok(body)
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
