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

use docopt::Docopt;
use tempdir::TempDir;
use time::{SteadyTime, Duration, PreciseTime};
use notify::{RecommendedWatcher, Watcher};
use iron::Iron;
use staticfile::Static;
use ansi_term::Colour::Green;

use diecast::{Command, Site, Configuration, Rule, Plugin};
use diecast::support;

pub fn plugin() -> Plugin {
    Plugin::new("live", "Live preview of the site", Live::plugin)
}

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
    _temp_dir: TempDir,
    site: Site,
}

impl Live {
    pub fn new(mut site: Site) -> Live {
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
            site.configuration_mut().threads = jobs;
        }

        site.configuration_mut().is_preview = true;

        let temp_dir =
            TempDir::new(site.configuration_mut().output.file_name().unwrap().to_str().unwrap())
                .unwrap();

        site.configuration_mut().output = temp_dir.path().to_path_buf();

        println!("output dir: {:?}", site.configuration_mut().output);

        Live {
            site: site,
            _temp_dir: temp_dir,
        }
    }

    pub fn plugin(site: Site) -> Box<Command> {
        Box::new(Live::new(site))
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
    fn run(&mut self) -> diecast::Result<()> {
        let (e_tx, e_rx) = channel();

        let _guard =
            Iron::new(Static::new(&self.site.configuration().output))
            .listen_with("0.0.0.0:5000", 4, iron::Protocol::Http)
            .unwrap();

        let target = self.site.configuration().input.clone();

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

        try!(self.site.build());

        println!("finished building");

        let mut last_event = SteadyTime::now();
        let debounce = Duration::seconds(1);

        for (mut paths, tm) in e_rx.iter() {
            let delta = tm - last_event;

            // debounced; skip
            if delta < debounce {
                continue;
            }

            if let Some(ref pattern) = self.site.configuration().ignore {
                paths = paths.into_iter()
                    .filter(|p| !pattern.matches(&Path::new(p.file_name().unwrap())))
                    .collect::<HashSet<PathBuf>>();
            }

            if paths.is_empty() {
                continue;
            }

            let (mut ready, mut waiting): (HashSet<PathBuf>, HashSet<PathBuf>) =
                paths.into_iter().partition(|p| support::file_exists(p));

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
                    waiting.into_iter().partition(|p| support::file_exists(p));

                ready.extend(r.into_iter());
                waiting = w;
            }

            paths = ready;

            // TODO
            // this would probably become something like self.site.update();
            let paths = paths.into_iter()
            .map(|p|
                 support::path_relative_from(&p, &self.site.configuration().input)
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

            try!(self.site.build());

            let end = PreciseTime::now();

            println!("finished updating ({})", start.to(end));

            last_event = SteadyTime::now();
        }

        panic!("notification watcher disconnected");
    }
}
