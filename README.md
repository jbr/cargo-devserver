```manpage
cargo-devserver 0.2.0

USAGE:
    cargo devserver [FLAGS] [OPTIONS]

FLAGS:
    -h, --help       Prints help information
    -r, --release    use cargo build --release for an optimized production release
    -V, --version    Prints version information

OPTIONS:
    -b, --bin <bin>            the binary to execute. the default will be whatever cargo would execute [env: BIN=]
    -c, --cwd <cwd>            the working directory to execute cargo in. defaults to the current working directory
                               [default: {your current cwd}]
    -e, --example <example>    
    -o, --host <host>          Local host or ip to listen on [env: HOST=]  [default: localhost]
    -p, --port <port>          Local port to listen on [env: PORT=]  [default: 8080]
    -s, --signal <signal>       [default: SIGTERM]
    -w, --watch <watch>...     directories or files to watch in order to trigger a rebuild. directories will be watched
                               recursively [env: WATCH=]  [default: src]
```
