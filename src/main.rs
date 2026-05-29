use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::TargetKind;
use clap::Parser;
use nix::{
    sys::{
        signal::{self, Signal},
        socket::SockaddrStorage,
    },
    unistd::Pid,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGTERM},
    iterator::Signals,
};
use std::{
    io::{Write, stderr},
    net::{SocketAddr, ToSocketAddrs},
    os::fd::{AsRawFd, OwnedFd},
    path::{Path, PathBuf},
    process::{Command, exit},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Sender, channel},
    },
    thread::spawn,
    time::Duration,
};

mod cwd;

#[derive(clap::Args, Debug)]
#[command(author, version, about, long_about = None)]
pub struct DevServer {
    /// Local host or ip to listen on
    #[arg(short = 'o', long, env, default_value = "localhost")]
    host: String,

    /// Local port to listen on
    #[arg(short, long, env, default_value = "8080")]
    port: u16,

    /// directories or files to watch in order to trigger a rebuild. directories will be watched recursively
    #[arg(short, long, env, default_value = "src")]
    watch: Vec<PathBuf>,

    /// the binary to execute. the default will be whatever cargo would execute
    #[arg(short, long, env)]
    bin: Option<PathBuf>,

    /// the working directory to execute cargo in. defaults to the current working directory
    #[arg(short, long, default_value_t)]
    cwd: cwd::Cwd,

    /// use cargo build --release for an optimized production release
    #[arg(short, long, default_value_t)]
    release: bool,

    /// build the given example instead of the default binary
    #[arg(short, long)]
    example: Option<String>,

    /// the signal to send the child process when a fresh binary is ready
    #[arg(short, long, default_value = "SIGTERM")]
    signal: Signal,

    /// optimize the build/rebuild loop for the fastest possible compile times,
    /// even at the cost of runtime performance: disable debuginfo, maximize
    /// codegen-units, and use the cranelift codegen backend when it is available
    #[arg(short, long, default_value_t)]
    fast: bool,

    /// coalesce filesystem events that arrive within this many milliseconds into
    /// a single rebuild, so one editor save does not trigger several builds
    #[arg(long, env, default_value = "100")]
    debounce_ms: u64,
}

#[derive(Debug)]
enum Event {
    /// a fresh binary is on disk; forward the configured signal to the child
    Signal,
    /// watched sources changed; recompile
    Rebuild,
    /// the devserver itself was asked to terminate
    Shutdown,
}

impl DevServer {
    fn determine_bin(&self) -> Result<PathBuf> {
        if let Some(bin) = &self.bin {
            return bin
                .canonicalize()
                .with_context(|| format!("could not find --bin {}", bin.display()));
        }

        let metadata = cargo_metadata::MetadataCommand::new()
            .no_deps()
            .current_dir(&self.cwd)
            .exec()
            .context("could not read cargo metadata (is this a cargo project?)")?;

        let target_dir =
            metadata
                .target_directory
                .join(if self.release { "release" } else { "debug" });
        let target_dir = Path::new(target_dir.as_str());

        if let Some(example) = &self.example {
            return Ok(target_dir.join("examples").join(example));
        }

        let possible_bin_target_names = metadata
            .packages
            .iter()
            .filter_map(|p| match &p.default_run {
                Some(default_run) => Some(default_run.clone()),
                None => {
                    let bin_targets = p
                        .targets
                        .iter()
                        .filter(|t| t.kind.contains(&TargetKind::Bin))
                        .collect::<Vec<_>>();

                    match (p.manifest_path.parent(), &bin_targets[..]) {
                        (Some(dir), [only]) if dir == self.cwd => Some(only.name.clone()),
                        _ => None,
                    }
                }
            })
            .collect::<Vec<_>>();

        match &possible_bin_target_names[..] {
            [bin_target_name] => Ok(target_dir.join(bin_target_name)),
            [] => bail!(
                "could not determine which binary to run; pass --bin or --example, \
                 or set `default-run` in Cargo.toml"
            ),
            names => bail!(
                "found more than one candidate binary ({names:?}); pass --bin or --example, \
                 or set `default-run` in Cargo.toml"
            ),
        }
    }

    fn socket(addr: SocketAddr) -> nix::Result<OwnedFd> {
        use nix::sys::socket::*;
        let address_fam = if addr.is_ipv6() {
            AddressFamily::Inet6
        } else {
            AddressFamily::Inet
        };

        let fd = socket(
            address_fam,
            SockType::Stream,
            SockFlag::empty(),
            SockProtocol::Tcp,
        )?;
        setsockopt(&fd, sockopt::ReuseAddr, &true)?;
        bind(fd.as_raw_fd(), &SockaddrStorage::from(addr))?;
        listen(&fd, Backlog::MAXCONN)?;

        // The caller must keep this OwnedFd alive for the lifetime of the
        // process: it is the listening socket the child inherits via LISTEN_FD,
        // and it has to survive every child restart. Dropping it here would
        // close the socket out from under the child (this used to be a bug).
        Ok(fd)
    }

    fn open_socket(&self) -> Result<OwnedFd> {
        let addrs = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .with_context(|| format!("could not resolve {}:{}", self.host, self.port))?;

        let mut last_err = None;
        for addr in addrs {
            match Self::socket(addr) {
                Ok(fd) => return Ok(fd),
                Err(e) => last_err = Some(e),
            }
        }

        Err(last_err.map_or_else(
            || anyhow!("{}:{} resolved to no addresses", self.host, self.port),
            anyhow::Error::from,
        ))
        .with_context(|| format!("unable to bind to {}:{}", self.host, self.port))
    }

    /// Apply compile-speed overrides to the build command when `--fast` is set.
    ///
    /// Profile settings are injected via `CARGO_PROFILE_*` environment variables
    /// so the user's `Cargo.toml` is never touched, and the cranelift codegen
    /// backend is enabled only when it is actually installed on a nightly cargo.
    fn configure_fast(&self, build: &mut Command, args: &mut Vec<String>) {
        if !self.fast {
            return;
        }

        let profile = if self.release { "RELEASE" } else { "DEV" };
        for (key, value) in [
            ("DEBUG", "false"),
            ("OPT_LEVEL", "0"),
            ("CODEGEN_UNITS", "256"),
            ("INCREMENTAL", "true"),
        ] {
            build.env(format!("CARGO_PROFILE_{profile}_{key}"), value);
        }

        if cranelift_available() && cargo_is_nightly() {
            args.push("-Zcodegen-backend".into());
            build.env(
                format!("CARGO_PROFILE_{profile}_CODEGEN_BACKEND"),
                "cranelift",
            );
            log::info!("--fast: using the cranelift codegen backend and a speed-tuned profile");
        } else {
            log::info!(
                "--fast: speed-tuned profile only (cranelift unavailable; install with \
                 `rustup component add rustc-codegen-cranelift-preview` on a nightly toolchain)"
            );
        }
    }

    pub fn run(mut self) -> Result<()> {
        env_logger::init();

        let bin = self.determine_bin()?;

        // Held for the whole process: the child inherits this listening socket
        // by number via LISTEN_FD, and it must outlive every child restart.
        let socket = self.open_socket()?;
        let socket_fd = socket.as_raw_fd();

        if let Ok(sockname) = nix::sys::socket::getsockname::<SockaddrStorage>(socket_fd) {
            log::info!(
                "bound tcp://{}:{} as tcp://{sockname}",
                self.host,
                self.port
            );
        }

        let mut run = Command::new(&bin);
        run.env("LISTEN_FD", socket_fd.to_string());
        run.env("CARGO_DEVSERVER", "true");
        run.current_dir(&self.cwd);

        let mut build = Command::new("cargo");
        let mut args = vec!["build".to_string(), "--color=always".to_string()];
        if self.release {
            args.push("--release".into());
        }
        if let Some(example) = &self.example {
            args.push("--example".into());
            args.push(example.clone());
            self.watch.push(self.cwd.join("examples"));
        }
        self.configure_fast(&mut build, &mut args);
        build.env("CARGO_DEVSERVER", "true");
        build.args(&args);
        build.current_dir(&self.cwd);

        let mut child = run.spawn().context("could not spawn the child process")?;
        let child_id = Arc::new(Mutex::new(child.id()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let signal = self.signal;
        let debounce = Duration::from_millis(self.debounce_ms);

        let (tx, rx) = channel();

        spawn_signal_listener(tx.clone());
        self.spawn_watcher(tx, bin.clone(), debounce)?;

        {
            let child_id = child_id.clone();
            let shutdown = shutdown.clone();
            spawn(move || {
                loop {
                    let exit_status = child.wait().expect("could not wait on child process");
                    if shutdown.load(Ordering::SeqCst) {
                        log::info!("shutting down");
                        exit(exit_status.code().unwrap_or_default());
                    }
                    log::info!("child shut down, restarting");
                    child = run.spawn().expect("could not respawn the child process");
                    *child_id.lock().unwrap() = child.id();
                }
            });
        }

        let send_signal = |signal: Signal| {
            let pid = Pid::from_raw(*child_id.lock().unwrap() as i32);
            if let Err(e) = signal::kill(pid, signal) {
                log::warn!("could not send {signal} to child {pid}: {e}");
            }
        };

        loop {
            match rx.recv()? {
                Event::Signal => {
                    log::info!("attempting to send {signal}");
                    send_signal(signal);
                }

                Event::Rebuild => {
                    log::info!("building...");
                    match build.output() {
                        Ok(output) if output.status.success() => {
                            log::debug!("{}", String::from_utf8_lossy(&output.stdout));
                        }
                        Ok(output) => {
                            stderr().write_all(&output.stderr).ok();
                        }
                        Err(e) => log::error!("could not run cargo build: {e}"),
                    }
                }

                Event::Shutdown => {
                    // Flag first so the wait-thread treats the child's imminent
                    // exit as a shutdown, then actually ask the child to stop —
                    // a bare SIGINT/SIGTERM to the devserver otherwise leaves
                    // the child running.
                    shutdown.store(true, Ordering::SeqCst);
                    send_signal(Signal::SIGTERM);
                }
            }
        }
    }

    fn spawn_watcher(&self, tx: Sender<Event>, bin: PathBuf, debounce: Duration) -> Result<()> {
        let (raw_tx, raw_rx) = channel::<notify::Event>();
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<notify::Event>| {
                if let Ok(event) = result {
                    let _ = raw_tx.send(event);
                }
            },
            notify::Config::default().with_compare_contents(true),
        )
        .context("could not create filesystem watcher")?;

        for watch in &self.watch {
            let watch = if watch.is_relative() {
                self.cwd.join(watch)
            } else {
                watch.clone()
            };
            let watch = watch.canonicalize().unwrap_or(watch);
            log::info!("watching {}", watch.display());
            watcher
                .watch(&watch, RecursiveMode::Recursive)
                .with_context(|| format!("could not watch {}", watch.display()))?;
        }

        watcher
            .watch(&bin, RecursiveMode::NonRecursive)
            .with_context(|| format!("could not watch {}", bin.display()))?;

        spawn(move || {
            // keep the watcher alive for as long as we are receiving events
            let _watcher = watcher;
            while let Ok(event) = raw_rx.recv() {
                // Coalesce a burst of events (one save can touch many files)
                // into a single decision.
                let mut paths = event.paths;
                while let Ok(more) = raw_rx.recv_timeout(debounce) {
                    paths.extend(more.paths);
                }

                let mut touched_source = false;
                let mut touched_bin = false;
                for path in paths {
                    match path.canonicalize() {
                        Ok(path) if path == bin => touched_bin = true,
                        // a removed/renamed source file won't canonicalize, but
                        // it is still a reason to rebuild
                        _ => touched_source = true,
                    }
                }

                // Source edits win: the rebuild they trigger produces a fresh
                // binary, whose own change event then arrives alone and is
                // forwarded as a Signal.
                let event = if touched_source {
                    Event::Rebuild
                } else if touched_bin {
                    Event::Signal
                } else {
                    continue;
                };

                if tx.send(event).is_err() {
                    break;
                }
            }
        });

        Ok(())
    }
}

fn spawn_signal_listener(tx: Sender<Event>) {
    spawn(move || {
        let mut signals =
            Signals::new([SIGHUP, SIGTERM, SIGINT]).expect("could not install signal handler");
        for signal in signals.forever() {
            let event = match signal {
                SIGHUP => Event::Signal,
                _ => Event::Shutdown,
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
}

/// Is the cranelift codegen backend installed in the active sysroot?
fn cranelift_available() -> bool {
    let Ok(output) = Command::new("rustc").args(["--print", "sysroot"]).output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let sysroot = String::from_utf8_lossy(&output.stdout);
    let rustlib = Path::new(sysroot.trim()).join("lib").join("rustlib");

    let Ok(targets) = std::fs::read_dir(&rustlib) else {
        return false;
    };
    targets
        .flatten()
        .filter_map(|target| std::fs::read_dir(target.path().join("codegen-backends")).ok())
        .flatten()
        .flatten()
        .any(|backend| backend.file_name().to_string_lossy().contains("cranelift"))
}

/// Does the active `cargo` accept the unstable `-Z` flags cranelift needs?
fn cargo_is_nightly() -> bool {
    Command::new("cargo")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains("nightly"))
}

#[derive(clap::Parser, Debug)]
#[command(name = "cargo")]
#[command(bin_name = "cargo")]
pub enum CliRoot {
    Devserver(DevServer),
}

fn main() -> Result<()> {
    let CliRoot::Devserver(devserver) = CliRoot::parse();
    devserver.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        CliRoot::command().debug_assert();
    }

    #[test]
    fn parses_flags_and_options() {
        let CliRoot::Devserver(devserver) =
            CliRoot::parse_from(["cargo", "devserver", "--port", "9999", "--fast"]);
        assert_eq!(devserver.port, 9999);
        assert!(devserver.fast);
        assert_eq!(devserver.signal, Signal::SIGTERM);
        assert_eq!(devserver.debounce_ms, 100);
    }

    #[test]
    fn probes_do_not_panic() {
        // These shell out to rustc/cargo; just confirm they return cleanly.
        let _ = cranelift_available();
        let _ = cargo_is_nightly();
    }
}
