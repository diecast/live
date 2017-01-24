#[macro_use]
extern crate log;
extern crate diecast;
extern crate docopt;
extern crate tempdir;
extern crate time;
extern crate notify;
extern crate rustc_serialize;
extern crate ansi_term;

extern crate hyper;
extern crate url;
extern crate futures;

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
use ansi_term::Colour::Green;

use std::fs::File;
use std::io::Read;

use hyper::{Get, StatusCode};
use hyper::server::{Http, Service, Request, Response};

use url::percent_encoding::percent_decode;

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

struct FileServer {
    root_path: PathBuf,
}

impl FileServer {
    fn new(root_path: PathBuf) -> FileServer {
        FileServer {
            root_path: root_path,
        }
    }

    fn percent_decode(&self, input: &str) -> String {
        percent_decode(input.as_bytes()).decode_utf8().unwrap().into_owned()
    }

    // Perform a component-normalization of the given path. This doesn't
    // actually really normalize it by consulting the file system.
    fn normalize_path(&self, path: &Path) -> PathBuf {
        use std::path::Component;

        path.components()
            .fold(PathBuf::new(), |mut result, p| {
                match p {
                    Component::Normal(x) => {
                        match x.to_str() {
                            Some(s) => result.push(self.percent_decode(s)),
                            None => result.push(x)
                        }

                        result
                    }
                    Component::ParentDir => {
                        result.pop();
                        result
                    },
                    _ => result
                }
            })
    }

    fn get_target_path(&self, requested_path: &Path) -> Option<PathBuf> {
        trace!("live: Requested path: {}", requested_path.display());

        let normalized = self.normalize_path(requested_path);
        trace!("live: Normalized path: {}", normalized.display());

        let target_path = self.root_path.join(normalized);
        trace!("live: Target path: {}", target_path.display());

        if target_path.is_file() {
            return Some(target_path.to_path_buf());
        }

        let index_path = target_path.join("index.html");
        trace!("live: Index path: {}", index_path.display());

        if index_path.is_file() {
            Some(index_path.to_path_buf())
        } else {
            None
        }
    }
}

impl Service for FileServer {
    type Request = Request;
    type Response = Response;
    type Error = hyper::Error;
    type Future = ::futures::Finished<Response, hyper::Error>;

    fn call(&self, req: Request) -> Self::Future {
        let response: Response = match req.method() {
            &Get => {
                match self.get_target_path(Path::new(req.path())) {
                    Some(path) => {
                        // TODO
                        // audit path
                        let mut f = File::open(path).unwrap();
                        let mut v = Vec::new();

                        // TODO
                        // Preferably don't read everything into memory first. This is
                        // done currently because there doesn't seem to be a way to
                        // stream the file in Hyper yet.
                        f.read_to_end(&mut v).unwrap();

                        Response::new().with_body(v)
                    }
                    None => {
                        Response::new().with_status(StatusCode::NotFound)
                    }
                }

            }
            _ => Response::new().with_status(StatusCode::NotFound),
        };

        futures::finished(response)
    }
}

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

impl Command for Live {
    fn description(&self) -> &'static str {
        "Live preview of the site"
    }

    fn run(&mut self, site: &mut Site) -> diecast::Result<()> {
        let _temp_dir = self.configure(site.configuration_mut());

        let (e_tx, e_rx) = channel();

        let output = site.configuration().output.clone();
        let address = self.address.clone();

        thread::spawn(move || {
            let server = Http::new().bind(&address, move || Ok(FileServer::new(output.clone()))).unwrap();
            println!("Listening on http://{}", server.local_addr().unwrap());
            server.run().unwrap();
        });

        let target = site.configuration().input.clone();
        let input = site.configuration().input.canonicalize().unwrap();

        thread::spawn(move || {
            let (tx, rx) = channel();
            let watcher: notify::Result<RecommendedWatcher> = Watcher::new(tx, ::std::time::Duration::from_millis(10));

            match watcher {
                Ok(mut watcher) => {
                    match watcher.watch(&target, notify::RecursiveMode::Recursive) {
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
                                use notify::DebouncedEvent::*;

                                match event {
                                    NoticeWrite(path) => {
                                        trace!("live: file notification: NoticeWrite {}", path.display());
                                    },
                                    NoticeRemove(path) => {
                                        trace!("live: file notification: NoticeRemove {}", path.display());
                                    },
                                    Create(path) => {
                                        trace!("live: file notification: Create {}", path.display());

                                        let now = SteadyTime::now();
                                        let is_contained = set.contains(&path);

                                        // past rebounce period
                                        if (now - last_bounce) > rebounce {
                                            // past rebounce period, send events
                                            if !set.is_empty() {
                                                e_tx.send((set, now)).unwrap();
                                                set = HashSet::new();
                                            }
                                        }

                                        // within rebounce period
                                        if !is_contained {
                                            last_bounce = now;
                                            // add path and extend rebounce
                                            set.insert(path);
                                        }
                                    },
                                    Write(path) => {
                                        trace!("live: file notification: Write {}", path.display());

                                        let now = SteadyTime::now();
                                        let is_contained = set.contains(&path);

                                        // past rebounce period
                                        if (now - last_bounce) > rebounce {
                                            // past rebounce period, send events
                                            if !set.is_empty() {
                                                e_tx.send((set, now)).unwrap();
                                                set = HashSet::new();
                                            }
                                        }

                                        // within rebounce period
                                        if !is_contained {
                                            last_bounce = now;
                                            // add path and extend rebounce
                                            set.insert(path);
                                        }
                                    },
                                    Chmod(path) => {
                                        trace!("live: file notification: Chmod {}", path.display());

                                        let now = SteadyTime::now();
                                        let is_contained = set.contains(&path);

                                        // past rebounce period
                                        if (now - last_bounce) > rebounce {
                                            // past rebounce period, send events
                                            if !set.is_empty() {
                                                e_tx.send((set, now)).unwrap();
                                                set = HashSet::new();
                                            }
                                        }

                                        // within rebounce period
                                        if !is_contained {
                                            last_bounce = now;
                                            // add path and extend rebounce
                                            set.insert(path);
                                        }
                                    },
                                    Remove(path) => {
                                        trace!("live: file notification: Remove {}", path.display());

                                        let now = SteadyTime::now();
                                        let is_contained = set.contains(&path);

                                        // past rebounce period
                                        if (now - last_bounce) > rebounce {
                                            // past rebounce period, send events
                                            if !set.is_empty() {
                                                e_tx.send((set, now)).unwrap();
                                                set = HashSet::new();
                                            }
                                        }

                                        // within rebounce period
                                        if !is_contained {
                                            last_bounce = now;
                                            // add path and extend rebounce
                                            set.insert(path);
                                        }
                                    },
                                    Rename(from, to) => {
                                        trace!("live: file notification: renamed {} to {}",
                                               from.display(), to.display());

                                        let now = SteadyTime::now();
                                        let is_contained = set.contains(&from);

                                        // past rebounce period
                                        if (now - last_bounce) > rebounce {
                                            // past rebounce period, send events
                                            if !set.is_empty() {
                                                e_tx.send((set, now)).unwrap();
                                                set = HashSet::new();
                                            }
                                        }

                                        // within rebounce period
                                        if !is_contained {
                                            last_bounce = now;
                                            // add path and extend rebounce
                                            set.insert(from);
                                        }

                                        let now = SteadyTime::now();
                                        let is_contained = set.contains(&to);

                                        // past rebounce period
                                        if (now - last_bounce) > rebounce {
                                            // past rebounce period, send events
                                            if !set.is_empty() {
                                                e_tx.send((set, now)).unwrap();
                                                set = HashSet::new();
                                            }
                                        }

                                        // within rebounce period
                                        if !is_contained {
                                            last_bounce = now;
                                            // add to and extend rebounce
                                            set.insert(to);
                                        }
                                    }
                                    Rescan => {
                                        trace!("live: file notification: rescaning");
                                    }
                                    Error(err, path) => {
                                        println!("notification error from path `{:?}`: {}", path, err);

                                        ::std::process::exit(1);
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
                    println!("could not create watcher: {}", e);

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
            .map(|p| p.strip_prefix(&input).unwrap().to_path_buf())
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
