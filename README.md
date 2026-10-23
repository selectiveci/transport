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
```

`--stdio` reads the URL from `SELECTIVE_TRANSPORT_URL` or, if that is unset, from the first stdin line. The API key is not an argument. Client messages are JSON lines on stdin. `exit` shuts the process down and is not forwarded. Server commands are JSON lines on stdout. `--stdio` and the FIFO command (newline-delimited JSON on `/tmp/<runner_id>_1` and `_2`) run the same transport: text frames include the trailing newline, WebSocket control frames are not written out, and the transport moves its own connection when the server asks.

When a `--stdio` connection ends, stderr says how: `websocket closed by the server`, `websocket connection dropped`, or `websocket keepalive timeout: ...`.

In-process callers use `Session` on a Tokio runtime or `BlockingSession` from any thread, plus `snapshot_tree`, `upload_fragment`, and `detect_build_env`.

| Binding | Use |
| --- | --- |
| Python, JavaScript | `BlockingSession` or `Session` when the wheel loads; otherwise `--stdio` |
| Elixir, PHP, Java | `--stdio` |
| Ruby | `<url> <runner_id>` until it moves |

### Fragment upload

`--upload-fragment` posts the gzip body on stdin to `/test_maps/fragments` under the host, keeping the host's path: `ws://h/prefix` posts to `http://h/prefix/test_maps/fragments`. The API key is `SELECTIVE_TEST_MAP_UPLOAD_KEY`. The host comes from `detect_build_env` (`SELECTIVE_HOST`, otherwise `wss://app.selective.ci`). Each attempt connects within 10 s and fails if the server goes 120 s without sending anything.

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

Keepalive is off unless the client sets `SELECTIVE_WS_PING_INTERVAL_MS`, and the FIFO transport never pings. When it is set, `--stdio`, `Session`, and `BlockingSession` send a WebSocket ping at that interval, and treat the connection as dead once nothing at all has arrived from the server for the interval plus `SELECTIVE_WS_PONG_TIMEOUT_MS` (default `45000`). With a `20000` interval that is 65s, longer than the 30s a busy server can take to read a test manifest. A send that is waiting on a server that has stopped reading fails only at that same deadline. The caller reconnects as it would after a dropped connection.

### Conformance

The shared scenario suite is its own binary, `transport-conformance`, built next to `transport` by `cargo build --release`. A client's CI runs:

```text
transport-conformance run --client 'COMMAND' --fixture-ids ids.json --transport /path/to/transport
```

The transport is `--transport`, else `SELECTIVE_TRANSPORT`, else the `transport` next to `transport-conformance`. The client command gets it as `SELECTIVE_TRANSPORT`. Without `--client`, the built-in reference client (`transport-conformance client`) runs every scenario, and `--via direct|stdio|fifo|both|all` picks how it connects. `both` is direct and stdio; `all` (the default) adds FIFO, which is unix-only. A FIFO trial makes a `/tmp/conformance-<pid>-<n>_1` and `_2` pipe pair, runs `transport <url> <id>`, and removes the pipes afterwards. FIFO mode has no keepalive, so the two keepalive scenarios are `n/a` over FIFO. On stdio and FIFO trials every frame the mock server receives must end in `\n`. `transport-conformance list` prints the scenario names.

Run the suite from inside a git checkout: a scenario that expects a `test_map_tree` fails, naming the checkout, when it isn't one.

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
| `SELECTIVE_TRANSPORT` | the transport binary, when one was found |
| `SELECTIVE_PLATFORM`, `SELECTIVE_BRANCH`, `SELECTIVE_SHA`, `SELECTIVE_GIT_REPO`, `SELECTIVE_GIT_PROVIDER`, `SELECTIVE_PR_TITLE` | fixed conformance values, only when the caller left them unset |

`busy_server` removes `SELECTIVE_WS_PING_INTERVAL_MS` and `SELECTIVE_WS_PONG_TIMEOUT_MS`, stops reading for 3 s, and requires the results on the same connection afterwards.

A scenario that needs a recording client (`requires_record`) reports `n/a` unless the client's connect URL advertises `test_map=record`. A scenario can list languages it doesn't apply to yet (`not_for_languages`, matched against the connect URL's `language`): `queued_before_connected` and `signal_keeps_close_status` are `n/a` for `ruby`.

Every `test_cases` entry in a manifest needs a numeric `run_time` and a non-empty `file_path` that isn't absolute. `print_notice` and `print_message` text must appear in the client's output. A frame the client can't parse must make it exit 1 (`unparseable_frame`).

`--fixture-ids FILE` (or `SELECTIVE_CONFORMANCE_FIXTURE_IDS`) maps roles to the client's test ids. Missing roles keep the built-in literals. The manifest order is `pass`, `fail`, `pending`, `flaky`.

```json
{"pass":"spec.ok","fail":"spec.bad","pending":"spec.skip","flaky":"spec.flake"}
```

`group_hook_time` checks the client's group-hook timing. Give the `pass` case's group a before-all hook that sleeps a known time, and declare it as a number:

```json
{"pass":"spec.ok","before_all_seconds":0.2}
```

The `report_at_finish` values whose keys start `seconds_in_` and end `_hooks` must then total between half the declared time and five times it plus 1 s. Without `before_all_seconds` the scenario is `n/a`.

The harness rewrites scenario ids before it sends or expects them. The built-in client reads the same file so it can emit those ids. A language client should emit its native ids and ignore `SELECTIVE_CONFORMANCE_FIXTURE_IDS`. Forward `SELECTIVE_WS_PING_INTERVAL_MS` and `SELECTIVE_WS_PONG_TIMEOUT_MS` to a child `transport --stdio`, or the idle scenario will wait on a cadence the child is not using.

`idle_keepalive` keeps the mock quiet for three ping intervals and requires the connection to stay up and deliver a ping. `pong_timeout` stops answering pongs and requires a clean `reconnect=true` dial. `close_frame` sends a bare Close frame, which counts as a lost connection (`connection_retries: 1`). `reconnect_then_going_away` sends `reconnect` and then Close 1001, as the server does, which does not count. `retries_across_reconnects` drops the connection twice across successful reconnects and expects `connection_retries: 2`. Scenario files live in `conformance/scenarios`.

## Release platforms

The Linux x86_64 gnu binary is built on the Ubuntu 24.04 runner in `.github/workflows/pre-release.yml`. On 2026-10-21 Nate set that binary's glibc floor at 2.39 (hard requirement `GLIBC_2.34`, weak requirement `GLIBC_2.39`). That decision supersedes `fda9533` (2023-11-03), which pinned Linux builds to Ubuntu 20.04 after a `GLIBC_2.33' not found` failure. The release workflow stays on Ubuntu 24.04. musl release binaries are static-pie. The aarch64 gnu binary is cross-built and keeps the glibc of that image.

## License

The package is available as open source under the terms of the MIT License.
