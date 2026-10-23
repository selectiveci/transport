#![warn(clippy::perf, clippy::style, clippy::pedantic)]

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
        None => {
            eprintln!("{USAGE}");
            Ok(1)
        }
        Some("--help" | "-h") => {
            eprintln!("{USAGE}");
            Ok(0)
        }
        Some("--stdio") => Ok(block_on(transport::stdio::run())),
        Some("--snapshot") => snapshot_main(args.next()),
        Some("--upload-fragment") => upload_main(args.collect()),
        Some("--build-env") => {
            println!("{}", serde_json::to_string(&transport::detect_build_env())?);
            Ok(0)
        }
        Some("conformance") => conformance_main(args.collect()),
        Some("conformance-client") => Ok(conformance_client_main(
            args.any(|arg| arg == "--via-stdio"),
        )),
        Some(url) => match args.next() {
            Some(runner_id) => Ok(block_on_abandoning_blocking(transport::run_fifo(
                url, &runner_id,
            ))),
            None => {
                eprintln!("{USAGE}");
                Ok(1)
            }
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
        None => {
            eprintln!("not a git checkout");
            Ok(1)
        }
    }
}

fn upload_main(args: Vec<String>) -> Result<i32, transport::Error> {
    let mut flag_run_id = String::new();
    let mut flag_run_attempt = String::new();
    let mut flag_runner_id = String::new();
    let mut index = 0;
    while index < args.len() {
        let value = match args.get(index + 1) {
            Some(value) if !value.is_empty() => value.clone(),
            _ => {
                eprintln!("missing value for {}", args[index]);
                return Ok(1);
            }
        };
        match args[index].as_str() {
            "--run-id" => flag_run_id = value,
            "--run-attempt" => flag_run_attempt = value,
            "--runner-id" => flag_runner_id = value,
            other => {
                eprintln!("unknown --upload-fragment argument {other}\n{USAGE}");
                return Ok(1);
            }
        }
        index += 2;
    }
    let raw = read_stdin()?;
    if raw.is_empty() {
        eprintln!("--upload-fragment expected a gzip body on stdin");
        return Ok(1);
    }
    let (payload, gzip_body) = split_fragment_stdin(&raw)?;
    if gzip_body.is_empty() {
        eprintln!("--upload-fragment expected a gzip body on stdin");
        return Ok(1);
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
    match block_on(upload_fragment(&upload)) {
        Ok(()) => Ok(0),
        Err(err) => {
            eprintln!("Selective could not upload this runner's test map ({err}).");
            Ok(1)
        }
    }
}

fn conformance_main(args: Vec<String>) -> Result<i32, transport::Error> {
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
                scenario = args.get(index).cloned();
            }
            "--via" => {
                index += 1;
                via = match args.get(index).map(String::as_str) {
                    Some("direct") => SuiteVia::Direct,
                    Some("stdio") => SuiteVia::Stdio,
                    Some("both") | None => SuiteVia::Both,
                    Some(other) => {
                        eprintln!("unknown --via {other}");
                        return Ok(1);
                    }
                };
            }
            "--client" => {
                index += 1;
                match args.get(index).filter(|value| !value.is_empty()) {
                    Some(value) => client = Some(value.clone()),
                    None => {
                        eprintln!("missing value for --client");
                        return Ok(1);
                    }
                }
            }
            "--fixture-ids" => {
                index += 1;
                match args.get(index).filter(|value| !value.is_empty()) {
                    Some(value) => fixture_file = Some(value.clone()),
                    None => {
                        eprintln!("missing value for --fixture-ids");
                        return Ok(1);
                    }
                }
            }
            other => {
                eprintln!("unknown conformance argument {other}\n{USAGE}");
                return Ok(1);
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
            match block_on(run_suite_with(&bin, run)) {
                Ok(()) => Ok(0),
                Err(err) => {
                    eprintln!("{err}");
                    Ok(1)
                }
            }
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(1)
        }
    }
}

fn conformance_client_main(via_stdio: bool) -> i32 {
    let url = match std::env::var("SELECTIVE_TRANSPORT_URL") {
        Ok(url) if !url.is_empty() => url,
        _ => {
            eprintln!("conformance-client requires SELECTIVE_TRANSPORT_URL");
            return 1;
        }
    };
    block_on(transport::reference_client::run(RunOpts {
        url,
        via: if via_stdio { Via::Stdio } else { Via::Direct },
        catch_signals: true,
    }))
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

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(future)
}

fn block_on_abandoning_blocking<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let output = runtime.block_on(future);
    runtime.shutdown_background();
    output
}
