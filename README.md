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
transport <websocket-url> <runner-id>     # legacy FIFOs /tmp/<runner_id>_1 and _2
SELECTIVE_TRANSPORT_URL=wss://... transport --stdio
transport --snapshot [dir]
transport --upload-fragment               # gzip on stdin
transport --build-env
transport conformance run [--via direct|stdio|both]
```

`--stdio` reads the URL from `SELECTIVE_TRANSPORT_URL` or, if that is unset, from the first stdin line. The API key is not an argument. Client messages are JSON lines on stdin. `exit` shuts the process down and is not forwarded. Server commands are JSON lines on stdout.

In-process callers use `Session` on a Tokio runtime or `BlockingSession` from any thread, plus `snapshot_tree`, `upload_fragment`, and `detect_build_env`.

| Binding | Use |
| --- | --- |
| Python, JavaScript | `BlockingSession` or `Session` when the wheel loads; otherwise `--stdio` |
| Elixir, PHP, Java | `--stdio` |
| Ruby | `<url> <runner_id>` until it moves |

`transport conformance run` is the shared scenario suite. A client replaces the built-in `conformance-client` by speaking the same fixture protocol to the mock; the scenario files live in `conformance/scenarios`.

## License

The package is available as open source under the terms of the MIT License.
