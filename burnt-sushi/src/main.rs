#![feature(
    io_safety,
    once_cell,
    maybe_uninit_uninit_array,
    maybe_uninit_slice,
    let_chains
)]
#![warn(unsafe_op_in_unsafe_fn)]
#![allow(clippy::module_inception, non_snake_case)]
#![windows_subsystem = "windows"]

use std::{
    env, io,
    lazy::SyncLazy,
    mem,
    net::SocketAddrV4,
    path::{Path, PathBuf},
};

use clap::{ArgEnum, Parser};
use dll_syringe::{
    error::SyringeError,
    process::{OwnedProcessModule, Process},
    Syringe,
};
use log::{debug, error, info, warn};
use serde::Deserialize;
use spotify_process_scanner::{SpotifyInfo, SpotifyProcessScanner};
use tokio::{runtime, task::LocalSet};

use crate::{console::Console, named_mutex::NamedMutex, spotify_process_scanner::SpotifyState};

mod console;
mod named_mutex;
mod rpc;
mod spotify_process_scanner;
mod tray;

const APP_NAME: &str = "BurntSushi";
const APP_AUTHOR: &str = "OpenByteDev";
// const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_NAME_WITH_VERSION: &str = concat!("BurntSushi v", env!("CARGO_PKG_VERSION"));
const DEFAULT_BLOCKER_FILE_NAME: &str = "BurntSushiBlocker_x86.dll";
const DEFAULT_FILTER_FILE_NAME: &str = "filter.toml";

static ARGS: SyncLazy<Args> = SyncLazy::new(|| {
    // Try to attach console for printing errors during argument parsing.
    console::raw::attach();

    Args::parse()
});

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Show a console window with debug output.
    #[clap(long)]
    console: bool,

    /// Level of debug output.
    #[clap(long, arg_enum)]
    log_level: Option<LogLevel>,

    /// Start a new instance of this app even if one is already running.
    #[clap(long)]
    ignore_singleton: bool,

    /// Path to the blocker module.
    /// If the file doesn't exist it will be created with the default blocker.
    /// If not specified the app will try to find it in the same directory as the app with name `burnt-sushi-blocker-x86.dll` or write it to a temp file.
    #[clap(long)]
    blocker: Option<PathBuf>,

    /// Path to the filter config.
    /// If the file doesn't exist it will be created with the default config.
    /// If not specified the app will try to find it in the same directory as the app named `filter.toml`.
    #[clap(long)]
    filters: Option<PathBuf>,
}

#[derive(ArgEnum, Clone, Copy, Debug)]
enum LogLevel {
    Off,
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if ARGS.console {
        console::global::set(
            Console::attach()
                .or_else(Console::alloc)
                .unwrap_or_else(Console::none),
        );
    } else {
        console::global::set(Console::none());
    }

    let log_level = match ARGS.log_level.unwrap_or(LogLevel::Debug) {
        LogLevel::Off => log::LevelFilter::Off,
        LogLevel::Trace => log::LevelFilter::Trace,
        LogLevel::Debug => log::LevelFilter::Debug,
        LogLevel::Info => log::LevelFilter::Info,
        LogLevel::Warn => log::LevelFilter::Warn,
        LogLevel::Error => log::LevelFilter::Error,
    };
    log::set_max_level(log_level);

    info!("{}", APP_NAME_WITH_VERSION);

    if ARGS.ignore_singleton {
        run().await;
    } else {
        let lock = NamedMutex::new(&format!("{} SINGLETON MUTEX", APP_NAME)).unwrap();
        match lock.try_lock() {
            Ok(Some(_guard)) => run().await,
            Ok(None) => warn!("App is already running"),
            Err(e) => error!("Failed to lock singleton mutex: {}", e),
        };
    }

    console::global::unset();
}

async fn run() {
    let mut system_tray = tray::SystemTrayManager::build_and_run().await.unwrap();

    let mut app = App::new();
    tokio::select! {
        _ = app.run() => {
            unreachable!("App should never exit on its own");
        }
        _ = wait_for_ctrl_c() => {
            debug!("Ctrl-C received");
        }
        _ = system_tray.wait_for_exit() => {
            debug!("System tray exited");
        }
    }

    info!("Shutting down...");

    app.stop().await;
    system_tray.exit().await;

    info!("Exiting...");
}

async fn wait_for_ctrl_c() -> Result<(), ctrlc::Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut handler = Some(move || tx.send(()).unwrap());
    ctrlc::set_handler(move || {
        if let Some(h) = handler.take() {
            h()
        }
    })?;
    rx.await.unwrap();
    Ok(())
}

struct App {
    scanner: SpotifyProcessScanner,
    spotify_state: tokio::sync::watch::Receiver<SpotifyState>,
    state: AppState,
}

#[allow(clippy::large_enum_variant)]
enum AppState {
    Hooked(HookState),
    Unhooked,
}

struct HookState {
    syringe: Syringe,
    payload: OwnedProcessModule,
    rpc_task: async_thread::JoinHandle<()>,
}

impl App {
    fn new() -> Self {
        let (scanner, spotify_state) = SpotifyProcessScanner::new();
        Self {
            scanner,
            spotify_state,
            state: AppState::Unhooked,
        }
    }

    async fn run(&mut self) {
        tokio::select! {
            _ = self.scanner.run() => {
                unreachable!("Spotify scanner should never stop on its own");
            }
            _ = async {
                info!("Looking for Spotify...");
                while self.spotify_state.changed().await.is_ok() {
                    let state = self.spotify_state.borrow();
                    match *state {
                        SpotifyState::Running(ref spotify) => {
                            self.state.hook_spotify(spotify.try_clone().unwrap()).await;
                        },
                        SpotifyState::Stopped => {
                            self.state.unhook_spotify().await;
                            info!("Looking for Spotify...");
                        }
                    }
                }
            } => {}
        }
    }

    async fn stop(&mut self) {
        if matches!(self.state, AppState::Hooked(_)) {
            self.state.unhook_spotify().await;
        }
    }
}

impl AppState {
    async fn hook_spotify(&mut self, spotify: SpotifyInfo) {
        if let AppState::Hooked(_) = self {
            self.unhook_spotify().await;
        }

        info!("Found Spotify (PID={})", spotify.process.pid().unwrap());
        let syringe = Syringe::for_process(spotify.process);

        while let Some(prev_payload) = syringe
            .process()
            .find_module_by_name(DEFAULT_BLOCKER_FILE_NAME)
            .unwrap()
        {
            warn!("Found previously injected blocker");

            debug!("Stopping RPC of previous blocker");
            let stop_rpc =
                unsafe { syringe.get_payload_procedure::<fn()>(prev_payload, "stop_rpc") }
                    .unwrap()
                    .unwrap();
            match stop_rpc.call() {
                Ok(_) => {
                    debug!("Stopped RPC of previous blocker");
                }
                Err(e) => {
                    error!("Failed to stop RPC of previous blocker: {}", e);
                }
            }

            info!("Ejecting previous blocker...");
            syringe.eject(prev_payload).unwrap();

            info!("Ejected previous blocker");
        }

        info!("Loading filter config...");
        let filter_config = self.find_and_load_filter_config().await.unwrap();

        info!("Preparing blocker...");
        let payload_path = self.find_and_load_blocker().await.unwrap();

        info!("Injecting blocker...");
        let payload = syringe.inject(payload_path).unwrap();

        debug!("Starting RPC...");
        let start_rpc =
            unsafe { syringe.get_payload_procedure::<fn() -> SocketAddrV4>(payload, "start_rpc") }
                .unwrap()
                .unwrap();

        let rpc_socket_addr = start_rpc.call().unwrap();

        let rpc_task = async_thread::spawn(move || {
            let rt = runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let localset = LocalSet::new();
            localset.block_on(&rt, async move {
                rpc::run(rpc_socket_addr, filter_config).await.unwrap();
            });
        });

        info!("Blocker up and running!");
        *self = AppState::Hooked(HookState {
            payload: payload.try_to_owned().unwrap(),
            syringe,
            rpc_task,
        });
    }

    async fn unhook_spotify(&mut self) {
        let state = mem::replace(self, AppState::Unhooked);
        let state = match state {
            AppState::Hooked(state) => state,
            _ => return,
        };

        info!("Unhooking Spotify...");

        let result: Result<(), SyringeError> = async {
            let stop_rpc = unsafe {
                state
                    .syringe
                    .get_payload_procedure::<fn()>(state.payload.borrowed(), "stop_rpc")
            }?
            .unwrap();

            debug!("Stopping RPC...");
            stop_rpc.call()?;
            state.rpc_task.join().await.unwrap();
            debug!("Stopped RPC");

            if state.payload.process().is_alive() {
                info!("Ejecting blocker...");
                state.syringe.eject(state.payload.borrowed())?;
                info!("Ejected blocker");
            }

            Ok(())
        }
        .await;

        match result {
            Ok(_)
            | Err(SyringeError::ProcessInaccessible)
            | Err(SyringeError::ModuleInaccessible) => {}
            _ => todo!("{:#?}", result),
        };

        *self = AppState::Unhooked;
    }

    async fn find_and_load_blocker(&self) -> io::Result<PathBuf> {
        async fn try_load_blocker(
            path: &Path,
            check_len: bool,
            write_if_absent: bool,
        ) -> io::Result<()> {
            let payload_bytes =
                include_bytes!(concat!(env!("OUT_DIR"), "\\BurntSushiBlocker_x86.dll"));

            debug!("Looking for blocker at '{}'", path.display());
            if let Ok(metadata) = tokio::fs::metadata(path).await {
                if metadata.is_file() {
                    debug!("Found blocker at '{}'", path.display());
                    if check_len && metadata.len() != payload_bytes.len() as u64 {
                        debug!("Blocker at '{}' is incorrect size.", path.display());
                    } else {
                        return Ok(());
                    }
                }
            }
            if write_if_absent {
                debug!("Writing blocker to '{}'", path.display());
                tokio::fs::create_dir_all(path.parent().unwrap()).await?;
                tokio::fs::write(&path, payload_bytes).await?;
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "Blocker not found at given path.",
                ))
            }
        }

        debug!("Looking for blocker according to cli args...");
        if let Some(config_path) = &ARGS.blocker {
            if try_load_blocker(config_path, false, true).await.is_ok() {
                return Ok(config_path.to_path_buf());
            } else {
                debug!("Looking for blocker according to cli args...");
            }
        }

        debug!("Looking for blocker next to executable...");
        if let Some(sibling_path) = env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join(DEFAULT_BLOCKER_FILE_NAME)))
        {
            if try_load_blocker(&sibling_path, false, false).await.is_ok() {
                return Ok(sibling_path);
            }
        }

        debug!("Looking for existing blocker in temporary directory...");
        if let Some(temp_path) = env::temp_dir().parent().map(|p| {
            p.join(APP_AUTHOR)
                .join(APP_NAME_WITH_VERSION)
                .join(DEFAULT_BLOCKER_FILE_NAME)
        }) {
            if try_load_blocker(&temp_path, true, true).await.is_ok() {
                return Ok(temp_path);
            }
        }

        error!("Could not find or create blocker.");
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Could not find or create blocker.",
        ))
    }

    async fn find_and_load_filter_config(&self) -> io::Result<FilterConfig> {
        async fn try_load_filter_config_from_path(
            path: Option<&Path>,
            write_if_absent: bool,
        ) -> io::Result<FilterConfig> {
            let default_filter_bytes = include_str!(concat!(env!("OUT_DIR"), "\\filter.toml"));

            if let Some(path) = path {
                debug!("Looking for filter config at '{}'", path.display());
                if let Ok(filters) = tokio::fs::read_to_string(path).await {
                    debug!("Found filter config at '{}'", path.display());
                    try_load_filter_config_from_str(&filters)
                } else if write_if_absent {
                    debug!("Writing default filter config to '{}'", path.display());
                    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
                    tokio::fs::write(&path, default_filter_bytes).await?;
                    try_load_filter_config_from_str(default_filter_bytes)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "Filter config did not exist.",
                    ))
                }
            } else {
                debug!("Loading default filter config...");
                try_load_filter_config_from_str(default_filter_bytes)
            }
        }

        fn try_load_filter_config_from_str(filter_config: &str) -> io::Result<FilterConfig> {
            match toml::from_str(filter_config) {
                Ok(filter_config) => Ok(filter_config),
                Err(_) => {
                    warn!("Failed to parse filter config.");
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Filter config is invalid.",
                    ))
                }
            }
        }

        debug!("Looking for filter config according to cli args...");
        if let Some(config_path) = &ARGS.filters {
            if let Ok(filters) = try_load_filter_config_from_path(Some(config_path), true).await {
                return Ok(filters);
            }
        }

        debug!("Looking for filter config next to executable...");
        if let Some(sibling_path) = env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join(DEFAULT_FILTER_FILE_NAME)))
        {
            if let Ok(filters) = try_load_filter_config_from_path(Some(&sibling_path), false).await
            {
                return Ok(filters);
            }
        }

        try_load_filter_config_from_path(None, false).await
    }
}

#[derive(Deserialize, Debug)]
pub struct FilterConfig {
    allowlist: Vec<String>,
    denylist: Vec<String>,
}
