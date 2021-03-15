use nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};
use notify::{DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};
use signal_hook::{
    consts::signal::{SIGHUP, SIGUSR1},
    iterator::Signals,
};
use std::io::stderr;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::mpsc::channel;
use std::thread::spawn;
use std::{
    convert::TryInto,
    io::Write,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use structopt::StructOpt;

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
    #[structopt(short, long)]
    cwd: Option<PathBuf>,

    /// use cargo build --release for an optimized production release
    #[structopt(short, long)]
    release: bool,
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
            let metadata = Command::new("cargo")
                .current_dir(self.cwd.clone().unwrap())
                .args(&["metadata", "--format-version", "1"])
                .output()
                .unwrap();

            let value: serde_json::Value = serde_json::from_slice(&metadata.stdout).unwrap();
            let target_dir =
                PathBuf::from(value.get("target_directory").unwrap().as_str().unwrap());

            let root = value
                .get("resolve")
                .unwrap()
                .get("root")
                .unwrap()
                .as_str()
                .unwrap()
                .split(' ')
                .next()
                .unwrap();

            target_dir
                .join(if self.release { "release" } else { "debug" })
                .join(root)
                .canonicalize()
                .unwrap()
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
        bind(fd, &SockAddr::new_inet(InetAddr::from_std(&addr))).ok()?;
        listen(fd, 0).ok()?;

        log::info!("{}", getsockname(fd).ok()?);
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

        let cwd = self
            .cwd
            .get_or_insert_with(|| std::env::current_dir().unwrap())
            .clone();

        let bin = self.determine_bin();

        let mut run = Command::new(&bin);
        run.env("LISTEN_FD", socket.to_string());
        run.env("CARGO_DEVSERVER", "true");
        run.current_dir(&cwd);

        let mut build = Command::new("cargo");
        let mut args = vec!["build", "--color=always"];
        if self.release {
            args.push("--release");
        }
        build.env("CARGO_DEVSERVER", "true");
        build.args(&args[..]);
        build.current_dir(&cwd);

        let mut child = run.spawn().unwrap();
        let child_id = Arc::new(Mutex::new(child.id()));

        let (tx, rx) = channel();

        {
            let tx = tx.clone();
            spawn(move || {
                let mut signals = Signals::new(&[SIGHUP, SIGUSR1]).unwrap();

                loop {
                    for signal in signals.pending() {
                        if let SIGHUP = signal as libc::c_int {
                            tx.send(Event::Signal).unwrap();
                        }
                    }
                }
            });
        }

        spawn(move || {
            let (t, r) = channel::<DebouncedEvent>();
            let mut watcher = RecommendedWatcher::new(t, Duration::from_secs(1)).unwrap();

            if let Some(watches) = self.watch {
                for watch in watches {
                    let watch = if watch.is_relative() {
                        cwd.join(watch)
                    } else {
                        watch
                    };

                    let watch = watch.canonicalize().unwrap();
                    log::info!("watching {:?}", &watch);
                    watcher.watch(watch, RecursiveMode::Recursive).unwrap();
                }
            }

            watcher.watch(&bin, RecursiveMode::NonRecursive).unwrap();

            while let Ok(m) = r.recv() {
                let path = match &m {
                    DebouncedEvent::Create(p) => Some(p),
                    DebouncedEvent::Write(p) => Some(p),
                    DebouncedEvent::Chmod(p) => Some(p),
                    DebouncedEvent::Remove(p) => Some(p),
                    DebouncedEvent::Rename(_, p) => Some(p),
                    _ => None,
                };

                if let Some(path) = path {
                    let path = path.canonicalize().unwrap();

                    if path == bin {
                        tx.send(Event::Signal).unwrap();
                    } else {
                        tx.send(Event::Rebuild).unwrap();
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
                    signal::kill(
                        Pid::from_raw((*child_id.lock().unwrap()).try_into().unwrap()),
                        Signal::SIGTERM,
                    )
                    .unwrap();
                }
                Event::Rebuild => {
                    log::info!("building...");
                    let output = build.output();
                    while let Ok(x) = rx.try_recv() {
                        log::debug!("discarding {:?}", x);
                    }
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
