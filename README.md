# Selective Transport

This is the websocket transport used by Selective client libraries.

## Requirements

- [Rust](https://www.rust-lang.org/)

## Getting Started

### Installing Rust

To install Rust, follow the instructions from https://rustup.rs.

### Checking your Installation

1. To check your installation, open a new command prompt and run: `rustc --version`.
2. If Rust is installed correctly, you should see a response with the version number. 

### Installing Dependencies

This project uses Cargo, Rust's package manager, which should be installed with Rust by default.

Dependencies will be fetched automatically when building the transport (see below).

### Building the Project

#### Development Build

To build a development version of this project:

1. Navigate to the project root directory.
2. Run `cargo build`. This will compile the project with debug information included.

#### Release Build

To build a release version of this project:

1. Navigate to the project root directory.
2. Run `cargo build --release`. This will compile the project with optimizations.

## Usage

The crate is the WebSocket session. Language bindings call it in-process or spawn the binary. Ruby keeps the FIFO invocation.

```text
transport <websocket-url> <runner-id>
SELECTIVE_TRANSPORT_URL=wss://... transport --stdio
transport --snapshot [dir]
transport --upload-fragment [--run-id ID] [--run-attempt N] [--runner-id ID]
transport --build-env
transport conformance run [--scenario NAME] [--via direct|stdio|both]
                          [--client COMMAND] [--fixture-ids FILE]
```

`--stdio` reads the URL from `SELECTIVE_TRANSPORT_URL` or, if that is unset, from the first stdin line. The API key is not an argument. Client messages are JSON lines on stdin. `exit` shuts the process down and is not forwarded. Server commands are JSON lines on stdout. The legacy FIFO command is unchanged: newline-delimited JSON on `/tmp/<runner_id>_1` and `_2`, and text frames still include the trailing newline. WebSocket control frames are not written to that pipe.

In-process callers use `Session` on a Tokio runtime or `BlockingSession` from any thread, plus `snapshot_tree`, `upload_fragment`, and `detect_build_env`.

| Binding | Use |
| --- | --- |
| Python, JavaScript | `BlockingSession` or `Session` when the wheel loads; otherwise `--stdio` |
| Elixir, PHP, Java | `--stdio` |
| Ruby | `<url> <runner_id>` until it moves |

### Fragment upload

`--upload-fragment` posts the gzip body on stdin to `/test_maps/fragments`. The API key is `SELECTIVE_TEST_MAP_UPLOAD_KEY`. The host comes from `detect_build_env` (`SELECTIVE_HOST`, otherwise `wss://app.selective.ci`).

`run_id`, `run_attempt`, and `runner_id` are the session's values, not a fresh CI lookup. For each field the first present value wins:

1. The matching flag: `--run-id`, `--run-attempt`, `--runner-id`.
2. A JSON object on the first stdin line (the command payload), then the gzip bytes. Fields may be top-level or under `data`. `run_attempt` may be a string or a number. Example:

```text
{"run_id":"run-1","run_attempt":"2","runner_id":"shard"}
<gzip bytes>
```

3. `detect_build_env()`, which prefers `SELECTIVE_RUN_ID`, `SELECTIVE_RUN_ATTEMPT`, and `SELECTIVE_RUNNER_ID` over the CI provider's variables.

Pass the ids the WebSocket URL was opened with. A flag wins over the payload, and either wins over detection.

### Websocket keepalive

`Session`, `BlockingSession`, `--stdio`, and the FIFO transport send a WebSocket ping every `SELECTIVE_WS_PING_INTERVAL_MS` milliseconds (default `20000`, under a typical 60s proxy idle timeout). `0` disables pings. If no pong arrives within `SELECTIVE_WS_PONG_TIMEOUT_MS` (default `10000`), the socket closes and the caller reconnects through the same path as a dropped connection. `conformance-client` treats that close as connection loss and dials again with `reconnect=true`.

### Conformance

`transport conformance run` is the shared scenario suite. The built-in `conformance-client` is the default. `--via direct|stdio|both` applies only to that client (default `both`).

`--client COMMAND` (or `SELECTIVE_CONFORMANCE_CLIENT`) runs each scenario once against that command. The value is a command line split on whitespace, with quotes honored. The URL and key are not arguments. The child receives:

| Variable | Value |
| --- | --- |
| `SELECTIVE_TRANSPORT_URL` | full `ws://` URL, including `api_key`, `run_id`, `run_attempt`, `runner_id`, and JSON `metadata` |
| `SELECTIVE_API_KEY` | `conformance-key` |
| `SELECTIVE_HOST` | `ws://<mock host>:<port>` |
| `SELECTIVE_RUN_ID` | `run-1` |
| `SELECTIVE_RUN_ATTEMPT` | `1` |
| `SELECTIVE_RUNNER_ID` | `runner-1` |
| `SELECTIVE_TEST_MAP_UPLOAD_KEY` | `conformance-key` |
| `SELECTIVE_CONFORMANCE_FAST` | `1` |
| `SELECTIVE_WS_PING_INTERVAL_MS` | `200`, or `SELECTIVE_CONFORMANCE_PING_INTERVAL_MS` |
| `SELECTIVE_WS_PONG_TIMEOUT_MS` | `1000`, or `SELECTIVE_CONFORMANCE_PONG_TIMEOUT_MS` |

```text
transport conformance run --client '/path/to/client --suite' --fixture-ids ids.json
```

`--fixture-ids FILE` (or `SELECTIVE_CONFORMANCE_FIXTURE_IDS`) maps roles to the client's test ids. Missing roles keep the built-in literals. The manifest order is `pass`, `fail`, `pending`, `flaky`. Other keys are extra roles, substituted where a scenario contains `{{role}}`.

```json
{"pass":"spec.ok","fail":"spec.bad","pending":"spec.skip","flaky":"spec.flake"}
```

The harness rewrites scenario ids before it sends or expects them. `conformance-client` reads the same file so it can emit those ids. A language client should emit its native ids and ignore `SELECTIVE_CONFORMANCE_FIXTURE_IDS`. Forward `SELECTIVE_WS_PING_INTERVAL_MS` and `SELECTIVE_WS_PONG_TIMEOUT_MS` to a child `transport --stdio`, or the idle scenario will wait on a cadence the child is not using.

`idle_keepalive` keeps the mock quiet for three ping intervals (65s when the interval is the 20s default, which is longer than a 60s proxy idle timeout) and requires the connection to stay up and deliver a ping. `pong_timeout` stops answering pongs and requires a clean `reconnect=true` dial. Scenario files live in `conformance/scenarios`.

## License

The package is available as open source under the terms of the MIT License.
