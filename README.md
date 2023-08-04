# Selective Transport

This is the websocket transport used by Selective client libaries.

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

The transport is intended to be used alongside Selective client libraries.

## License

The package is available as open source under the terms of the MIT License.
