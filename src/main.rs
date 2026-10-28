#![warn(clippy::perf, clippy::style, clippy::pedantic)]

mod engine;

use std::path::PathBuf;

use transport::upload::{
    resolve_run_identity, split_fragment_stdin, upload_fragment, FragmentUpload,
};

const USAGE: &str = "\
usage:
  transport <websocket-url> <runner-id>
  transport --snapshot [dir]
  transport --upload-fragment [--run-id ID] [--run-attempt N] [--runner-id ID]
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
        Some("--upload-fragment") => upload_main(&args.collect::<Vec<_>>()),
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
