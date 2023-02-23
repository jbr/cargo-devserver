use nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use signal_hook::{
    consts::signal::{SIGHUP, SIGTERM},
    iterator::Signals,
};
use std::{
    convert::TryInto,
    io::{stderr, Write},
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
    process::Command,
    sync::{mpsc::channel, Arc, Mutex},
    thread::spawn,
};
use structopt::StructOpt;

mod cwd;

#[derive(StructOpt, Debug)]
pub struct DevServer {
    /// Local host or ip to listen on
    #[structopt(short = "o", long, env, default_value = "localhost")]
    host: String,

    /// Local port to listen on
    #[structopt(short, long, env, default_value = "8080")]
    port: u16,

    /// directories or files to watch in order to trigger a rebuild. directories will be watched recursively
    #[structopt(short, long, env, parse(from_os_str), default_value = "src")]
    watch: Option<Vec<PathBuf>>,

    /// the binary to execute. the default will be whatever cargo would execute
    #[structopt(short, long, env, parse(from_os_str))]
    bin: Option<PathBuf>,

    /// the working directory to execute cargo in. defaults to the current working directory
    #[structopt(short, long, default_value, parse(from_os_str))]
    cwd: cwd::Cwd,

    /// use cargo build --release for an optimized production release
    #[structopt(short, long)]
    release: bool,

    #[structopt(short, long)]
    example: Option<String>,

    #[structopt(short, long, default_value = "SIGTERM")]
    signal: Signal,
}

#[derive(Debug)]
enum Event {
    Signal,
    Rebuild,
}

impl DevServer {
    fn determine_bin(&self) -> PathBuf {
        if let Some(ref bin) = self.bin {
            bin.canonicalize().unwrap()
        } else {
            let metadata = cargo_metadata::MetadataCommand::new()
                .no_deps()
                .current_dir(&self.cwd)
                .exec()
                .unwrap();

            let target_dir = metadata
                .target_directory
                .join(if self.release { "release" } else { "debug" })
                .canonicalize()
                .unwrap();

            if let Some(example) = &self.example {
                return target_dir.join("examples").join(example);
            }

            let possible_bin_target_names = metadata
                .packages
                .iter()
                .filter_map(|p| match &p.default_run {
                    Some(x) => Some(x.clone()),
                    None => {
                        let bin_targets = p
                            .targets
                            .iter()
                            .filter(|t| t.kind.contains(&String::from("bin")))
                            .collect::<Vec<_>>();

                        if p.manifest_path.parent().unwrap() == self.cwd && bin_targets.len() == 1 {
                            Some(bin_targets[0].name.clone())
                        } else {
                            None
                        }
                    }
                })
                .collect::<Vec<_>>();

            let [bin_target_name] = &possible_bin_target_names[..] else {
                panic!("could not determine bin target {possible_bin_target_names:?}")
            };

            target_dir.join(bin_target_name)
        }
    }

    fn socket(addr: SocketAddr) -> Option<std::os::unix::io::RawFd> {
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
            Some(SockProtocol::Tcp),
        )
        .ok()?;
        setsockopt(fd, sockopt::ReuseAddr, &true).ok()?;
        bind(fd, &SockaddrStorage::from(addr)).ok()?;
        listen(fd, 0).ok()?;

        log::info!("{:?}", getsockname(fd).ok()?);
        Some(fd)
    }

    fn open_socket(&self) -> std::os::unix::io::RawFd {
        (&*self.host, self.port)
            .to_socket_addrs()
            .unwrap()
            .find_map(Self::socket)
            .unwrap()
    }

    pub fn run(mut self) {
        env_logger::init();

        let socket = self.open_socket();

        let bin = self.determine_bin();

        let mut run = Command::new(&bin);
        run.env("LISTEN_FD", socket.to_string());
        run.env("CARGO_DEVSERVER", "true");
        run.current_dir(&self.cwd);

        let mut build = Command::new("cargo");
        let mut args = vec!["build", "--color=always"];
        if self.release {
            args.push("--release");
        }
        if let Some(example) = &self.example {
            args.push("--example");
            args.push(example);
            self.watch
                .get_or_insert_with(Vec::new)
                .push(self.cwd.join("examples"));
        }
        build.env("CARGO_DEVSERVER", "true");
        build.args(&args[..]);
        build.current_dir(&self.cwd);

        let mut child = run.spawn().unwrap();
        let child_id = Arc::new(Mutex::new(child.id()));
        let signal = self.signal;

        let (tx, rx) = channel();

        {
            let tx = tx.clone();
            spawn(move || {
                let mut signals = Signals::new([SIGHUP, SIGTERM]).unwrap();

                loop {
                    for signal in signals.forever() {
                        if let SIGHUP | SIGTERM = signal as libc::c_int {
                            tx.send(Event::Signal).unwrap();
                        }
                    }
                }
            });
        }

        spawn(move || {
            let (t, r) = channel::<notify::Event>();
            let mut watcher = RecommendedWatcher::new(
                move |result: notify::Result<notify::Event>| {
                    t.send(result.unwrap()).unwrap();
                },
                notify::Config::default().with_compare_contents(true),
            )
            .unwrap();

            if let Some(watches) = self.watch {
                for watch in watches {
                    let watch = if watch.is_relative() {
                        self.cwd.join(watch)
                    } else {
                        watch
                    };

                    let watch = watch.canonicalize().unwrap_or(watch);
                    log::info!("watching {:?}", &watch);
                    watcher.watch(&watch, RecursiveMode::Recursive).unwrap();
                }
            }

            watcher.watch(&bin, RecursiveMode::NonRecursive).unwrap();

            while let Ok(m) = r.recv() {
                for path in m.paths {
                    if let Ok(path) = path.canonicalize() {
                        if path == bin {
                            tx.send(Event::Signal).unwrap();
                        } else {
                            tx.send(Event::Rebuild).unwrap();
                        }
                    }
                }
            }
        });

        {
            let child_id = child_id.clone();
            spawn(move || loop {
                child.wait().unwrap();
                log::info!("shut down, restarting");
                child = run.spawn().unwrap();
                *child_id.lock().unwrap() = child.id();
            });
        }

        loop {
            match rx.recv().unwrap() {
                Event::Signal => {
                    log::info!("attempting to send {}", &signal);
                    signal::kill(
                        Pid::from_raw((*child_id.lock().unwrap()).try_into().unwrap()),
                        signal,
                    )
                    .unwrap();
                }

                Event::Rebuild => {
                    log::info!("building...");
                    let output = build.output();
                    match output {
                        Ok(ok) => {
                            if ok.status.success() {
                                log::debug!("{}", String::from_utf8_lossy(&ok.stdout[..]));
                            } else {
                                stderr().write_all(&ok.stderr).unwrap();
                            }
                        }
                        Err(e) => {
                            eprintln!("{:?}", e);
                        }
                    }
                }
            }
        }
    }
}
#[derive(StructOpt)]
#[structopt(bin_name = "cargo")]
pub enum CliRoot {
    Devserver(DevServer),
}

impl CliRoot {
    pub fn run(self) {
        match self {
            CliRoot::Devserver(s) => s.run(),
        }
    }
}

fn main() {
    CliRoot::from_args().run();
}
