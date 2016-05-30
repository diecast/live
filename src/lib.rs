#[macro_use]
extern crate log;
extern crate diecast;
extern crate docopt;
extern crate tempdir;
extern crate time;
extern crate notify;
extern crate iron;
extern crate mount;
extern crate staticfile;
extern crate rustc_serialize;
extern crate ansi_term;

use std::sync::mpsc::{channel, TryRecvError};
use std::path::{Path, PathBuf};
use std::collections::HashSet;
use std::thread;
use std::error::Error;
use std::net::{ToSocketAddrs, SocketAddr};

use docopt::Docopt;
use tempdir::TempDir;
use time::{SteadyTime, Duration, PreciseTime};
use notify::{RecommendedWatcher, Watcher};
use iron::Iron;
use staticfile::Static;
use ansi_term::Colour::Green;

use diecast::{Command, Site, Configuration};

#[derive(RustcDecodable, Debug)]
struct Options {
    flag_jobs: Option<usize>,
    flag_verbose: bool,
}

static USAGE: &'static str = "
Usage:
    diecast live [options]

Options:
    -h, --help          Print this message
    -j N, --jobs N      Number of jobs to run in parallel
    -v, --verbose       Use verbose output
";

pub struct Live {
    address: SocketAddr,
}

impl Live {
    pub fn new<S>(address: S) -> Live
    where S: ToSocketAddrs {
        Live {
            address: address.to_socket_addrs().ok()
                     .and_then(|mut addrs| addrs.next())
                     .expect("Could not parse socket address."),
        }
    }

    pub fn configure(&mut self, configuration: &mut Configuration) -> TempDir {
        // 1. merge options into configuration; options overrides config
        // 2. construct site from configuration
        // 3. build site

        let docopt =
            Docopt::new(USAGE)
            .unwrap_or_else(|e| e.exit())
            .help(true);

        let options: Options = docopt.decode().unwrap_or_else(|e| {
            e.exit();
        });

        if let Some(jobs) = options.flag_jobs {
            configuration.threads = jobs;
        }

        configuration.is_preview = true;

        let temp_dir =
            TempDir::new(configuration.output.file_name().unwrap().to_str().unwrap())
                .unwrap();

        configuration.output = temp_dir.path().to_path_buf();

        println!("output dir: {:?}", configuration.output);
        temp_dir
    }
}

fn error_str(e: notify::Error) -> String {
    match e {
        notify::Error::Generic(e) => e.to_string(),
        notify::Error::Io(e) => e.to_string(),
        notify::Error::NotImplemented => String::from("Not Implemented"),
        notify::Error::PathNotFound => String::from("Path Not Found"),
        notify::Error::WatchNotFound => String::from("Watch Not Found"),
    }
}

impl Command for Live {
    fn description(&self) -> &'static str {
        "Live preview of the site"
    }

    fn run(&mut self, site: &mut Site) -> diecast::Result<()> {
        let _temp_dir = self.configure(site.configuration_mut());

        let (e_tx, e_rx) = channel();

        let _guard =
            Iron::new(Static::new(&site.configuration().output))
            .http(self.address.clone())
            .unwrap();

        let target = site.configuration().input.clone();

        thread::spawn(move || {
            let (tx, rx) = channel();
            let w: Result<RecommendedWatcher, notify::Error> = Watcher::new(tx);

            match w {
                Ok(mut watcher) => {
                    match watcher.watch(&target) {
                        Ok(_) => {},
                        Err(e) => {
                            println!("some error with the live command: {:?}", e);
                            ::std::process::exit(1);
                        },
                    }

                    let rebounce = Duration::milliseconds(10);
                    let mut last_bounce = SteadyTime::now();
                    let mut set: HashSet<PathBuf> = HashSet::new();

                    loop {
                        match rx.try_recv() {
                            Ok(event) => {
                                let now = SteadyTime::now();
                                let is_contained =
                                    event.path.as_ref()
                                    .map(|p| set.contains(p))
                                    .unwrap_or(false);

                                // past rebounce period
                                if (now - last_bounce) > rebounce {
                                    // past rebounce period, send events
                                    if !set.is_empty() {
                                        e_tx.send((set, now)).unwrap();
                                        set = HashSet::new();
                                    }
                                }

                                match event.op {
                                    Ok(op) => {
                                        trace!("event operation: {}",
                                            match op {
                                                ::notify::op::CHMOD => "chmod",
                                                ::notify::op::CREATE => "create",
                                                ::notify::op::REMOVE => "remove",
                                                ::notify::op::RENAME => "rename",
                                                ::notify::op::WRITE => "write",
                                                _ => "unknown",
                                        });
                                    },
                                    Err(e) => {
                                        println!(
                                            "notification error from path `{:?}`: {}",
                                            event.path,
                                            error_str(e));

                                        ::std::process::exit(1);
                                    }
                                }

                                // within rebounce period
                                if let Some(path) = event.path {
                                    if !is_contained {
                                        last_bounce = now;
                                        // add path and extend rebounce
                                        set.insert(path);
                                    }
                                }
                            },
                            Err(TryRecvError::Empty) => {
                                let now = SteadyTime::now();

                                if (now - last_bounce) > rebounce {
                                    last_bounce = now;

                                    // past rebounce period; send events
                                    if !set.is_empty() {
                                        e_tx.send((set, now)).unwrap();
                                        set = HashSet::new();
                                    }

                                    continue;
                                } else {
                                    thread::sleep_ms(10);
                                }
                            },
                            Err(TryRecvError::Disconnected) => {
                                panic!("notification manager disconnected");
                            },
                        }
                    }
                },
                Err(e) => {
                    println!("could not create watcher: {}", error_str(e));

                    ::std::process::exit(1);
                }
            }
        });

        try!(site.build());

        println!("finished building");

        let mut last_event = SteadyTime::now();
        let debounce = Duration::seconds(1);

        for (mut paths, tm) in e_rx.iter() {
            let delta = tm - last_event;

            // debounced; skip
            if delta < debounce {
                continue;
            }

            if let Some(ref pattern) = site.configuration().ignore {
                paths = paths.into_iter()
                    .filter(|p| !pattern.matches(&Path::new(p.file_name().unwrap())))
                    .collect::<HashSet<PathBuf>>();
            }

            if paths.is_empty() {
                continue;
            }

            let (mut ready, mut waiting): (HashSet<PathBuf>, HashSet<PathBuf>) =
                paths.into_iter().partition(|p| p.exists());

            // TODO optimize
            // so only non-existing paths are still polled?
            // perhaps using a partition
            while !waiting.is_empty() {
                // FIXME if user doesn't properly ignore files,
                // this can go on forever. instead, after a while,
                // this should just give up and remove the non-existing
                // file from the set

                // TODO: this should probably be thread::yield_now
                thread::park_timeout_ms(10);

                let (r, w): (HashSet<PathBuf>, HashSet<PathBuf>) =
                    waiting.into_iter().partition(|p| p.exists());

                ready.extend(r.into_iter());
                waiting = w;
            }

            paths = ready;

            // TODO
            // this would probably become something like self.site.update();
            let paths = paths.into_iter()
            .map(|p|
                 p.strip_prefix(&site.configuration().input)
                 .unwrap().to_path_buf())
            .collect::<HashSet<PathBuf>>();

            let modified_label: &'static str = "  Modified";

            if paths.len() == 1 {
                println!("{} {}",
                         Green.bold().paint(modified_label),
                         paths.iter().next().unwrap().display());
            } else {
                println!("{}", Green.bold().paint(modified_label));

                for path in &paths {
                    println!("    {}", path.display());
                }
            }

            let start = PreciseTime::now();

            try!(site.build());

            let end = PreciseTime::now();

            println!("finished updating ({})", start.to(end));

            last_event = SteadyTime::now();
        }

        panic!("notification watcher disconnected");
    }
}
