# cargo-devserver

A recompile harness for Rust web app development on `cfg(unix)`.

`cargo devserver` opens a listening TCP socket, hands it to your server process
via the `LISTEN_FD` environment variable, and watches your sources. When you
save a change it rebuilds in the background; when a fresh binary is ready it
signals the running process so the new build takes over — **without ever
dropping the listening socket**, so in-flight and incoming connections survive
the restart.

Your server is responsible for adopting the socket. For example:

```rust
use std::net::TcpListener;
use std::os::fd::{FromRawFd, RawFd};

let listener = match std::env::var("LISTEN_FD") {
    Ok(fd) => unsafe { TcpListener::from_raw_fd(fd.parse::<RawFd>().unwrap()) },
    Err(_) => TcpListener::bind("localhost:8080").unwrap(),
};
```

## Install

```sh
cargo install cargo-devserver
```

## Usage

```sh
cargo devserver [OPTIONS]
```

```manpage
recompile harness for rust web app development on cfg(unix)

Usage: cargo devserver [OPTIONS]

Options:
  -o, --host <HOST>                Local host or ip to listen on [env: HOST=] [default: localhost]
  -p, --port <PORT>                Local port to listen on [env: PORT=] [default: 8080]
  -w, --watch <WATCH>              directories or files to watch in order to trigger a rebuild. directories will be watched recursively [env: WATCH=] [default: src]
  -b, --bin <BIN>                  the binary to execute. the default will be whatever cargo would execute [env: BIN=]
  -c, --cwd <CWD>                  the working directory to execute cargo in. defaults to the current working directory [default: {your current cwd}]
  -r, --release                    use cargo build --release for an optimized production release
  -e, --example <EXAMPLE>          build the given example instead of the default binary
  -s, --signal <SIGNAL>            the signal to send the child process when a fresh binary is ready [default: SIGTERM]
  -f, --fast                       optimize the build/rebuild loop for the fastest possible compile times, even at the cost of runtime performance: disable debuginfo, maximize codegen-units, and use the cranelift codegen backend when it is available
      --debounce-ms <DEBOUNCE_MS>  coalesce filesystem events that arrive within this many milliseconds into a single rebuild, so one editor save does not trigger several builds [env: DEBOUNCE_MS=] [default: 100]
  -h, --help                       Print help
  -V, --version                    Print version
```

### `--fast`

`--fast` trades runtime performance for the shortest possible edit-rebuild
cycle. It disables debuginfo, maximizes `codegen-units`, keeps incremental
compilation on, and — when the [cranelift codegen backend][cranelift] is
installed on a nightly toolchain — compiles with cranelift. These settings are
applied through `CARGO_PROFILE_*` environment variables, so your `Cargo.toml`
is never modified. If cranelift is not available it is skipped (with a log
line) and the rest of the speed-tuned profile still applies.

To make cranelift available:

```sh
rustup component add rustc-codegen-cranelift-preview --toolchain nightly
```

[cranelift]: https://github.com/rust-lang/rustc_codegen_cranelift

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
