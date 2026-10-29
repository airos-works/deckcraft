//! DeckCraft for air/OS (and Haiku).
//!
//! The same interface as the desktop app (`deckcraft-ui-egui`), hosted natively: a
//! `BApplication` and a `BWindow` per egui viewport from the Be API (`native/host.cpp`,
//! `viewports`, `platform`) take the input and show the frames, which egui's output becomes with
//! Vulkan (`gpu`, shown by `present`) or, without a hardware Vulkan adapter, through a software
//! rasteriser (`raster`), with the system's file panels, clipboard, cursors, file types and
//! locale. eframe and winit don't run on Haiku; nothing below the interface changes.
//!
//! Usage: `DeckCraft [--sample] [--show] [--control PORT] [FILE…]`, as the desktop app.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
// Elsewhere only the platform-neutral parts build (and are tested).
#![cfg_attr(not(target_os = "haiku"), allow(dead_code))]

mod damage;
mod keys;
mod raster;
mod viewports;

#[cfg(target_os = "haiku")]
mod gpu;

#[cfg(target_os = "haiku")]
#[path = "../../deckcraft/src/control_server.rs"]
mod control_server;
#[cfg(target_os = "haiku")]
mod host;
#[cfg(target_os = "haiku")]
#[path = "../../deckcraft/src/logging.rs"]
mod logging;
#[cfg(target_os = "haiku")]
mod media_kit;
#[cfg(target_os = "haiku")]
mod platform;
#[cfg(target_os = "haiku")]
mod present;
#[cfg(target_os = "haiku")]
mod scanout;
#[cfg(target_os = "haiku")]
mod services;
#[cfg(target_os = "haiku")]
mod shell;

#[cfg(not(target_os = "haiku"))]
fn main() {
    let _ = std::io::Write::write_all(&mut std::io::stderr(), b"DeckCraft for air/OS runs on Haiku; elsewhere run `deckcraft`.\n");
    std::process::exit(1);
}

#[cfg(target_os = "haiku")]
use haiku::*;

#[cfg(target_os = "haiku")]
fn main() {
    haiku::main()
}

#[cfg(target_os = "haiku")]
mod haiku {
    use std::path::PathBuf;
    use std::rc::Rc;

    use deckcraft_engine::Session;
    use deckcraft_ui_egui::SlideApp;

    use crate::{control_server, host, logging, media_kit, services, shell};

    const USAGE: &str = "\
DeckCraft — the DeckCraft presentation editor for air/OS

USAGE:
  DeckCraft [--sample] [--show] [--control PORT] [FILE…]

OPTIONS:
  --sample            open the sample presentation
  --show              start the slide show
  --control PORT      start the localhost control server on PORT, for agents and scripts
                      (or set DECKCRAFT_CONTROL_PORT); see docs/control-protocol.md
  --version           print the version
  -h, --help          print this help
";

    /// The default window: content size in points, and the smallest (the desktop app's).
    const DEFAULT_SIZE: [f32; 2] = [1440.0, 900.0];
    const MIN_SIZE: [f32; 2] = [640.0, 420.0];

    /// The settings folder: `~/config/settings/DeckCraft`, beside other applications'. A copy
    /// started without `HOME` (by a server, say) uses Haiku's one home folder.
    fn config_dir() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map_or_else(|| PathBuf::from("/boot/home"), PathBuf::from);
        Some(home.join("config/settings/DeckCraft"))
    }

    /// Runs without preferences (`DECKCRAFT_NO_PREFS`, agents' test runs) neither read nor
    /// write them, and keep no log file.
    fn prefs_enabled() -> bool {
        std::env::var_os("DECKCRAFT_NO_PREFS").is_none()
    }

    fn log_dir() -> Option<PathBuf> {
        Some(config_dir()?.join(logging::LOG_DIR))
    }

    /// As the desktop app: `ui.json` (the interface) and `prefs.json` (the engine's options).
    fn load_prefs(app: &mut SlideApp) {
        if !prefs_enabled() {
            return;
        }
        let Some(dir) = config_dir() else { return };
        let (ui, prefs) = deckcraft_ui_egui::settings::load(&dir);
        if let Some(ui) = ui {
            app.ui = ui;
        }
        if let Some(prefs) = prefs {
            app.session.prefs = prefs;
        }
    }

    pub fn save_prefs(app: &SlideApp) {
        if prefs_enabled()
            && let Some(dir) = config_dir()
        {
            deckcraft_ui_egui::settings::save(&dir, &app.ui, &app.session.prefs);
        }
    }

    /// Where the window was when DeckCraft last quit: `window.json` in the settings folder (the
    /// editor's own settings files have no place for it), the frame's top-left corner and the
    /// content size in screen pixels.
    const WINDOW_FILE: &str = "window.json";

    fn saved_window() -> Option<([f32; 2], [f32; 2])> {
        if !prefs_enabled() {
            return None;
        }
        let bytes = std::fs::read(config_dir()?.join(WINDOW_FILE)).ok().filter(|b| b.len() < 4096)?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let num = |k: &str| v.get(k).and_then(serde_json::Value::as_f64).filter(|n| n.is_finite() && n.abs() < 1e6).map(|n| n as f32);
        let (x, y, w, h) = (num("x")?, num("y")?, num("width")?, num("height")?);
        (w >= 1.0 && h >= 1.0).then_some(([x, y], [w, h]))
    }

    /// Remember the window for the next run (see [`saved_window`]).
    pub fn save_window(frame: [f32; 4]) {
        if !prefs_enabled() {
            return;
        }
        let Some(dir) = config_dir() else { return };
        let [x, y, width, height] = frame;
        let json = serde_json::json!({"x": x, "y": y, "width": width, "height": height}).to_string();
        let path = dir.join(WINDOW_FILE);
        let tmp = dir.join(".window.json.tmp");
        let written = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&tmp, json)).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            log::warn!("{}: {e}", path.display());
        }
    }

    fn build_sha() -> &'static str {
        option_env!("DECKCRAFT_BUILD_SHA").map(|s| s.get(..8).unwrap_or(s)).unwrap_or("dev")
    }

    struct Args {
        sample: bool,
        show: bool,
        control: Option<u16>,
        files: Vec<String>,
    }

    fn args() -> Args {
        let mut a = Args {
            sample: false,
            show: false,
            control: std::env::var("DECKCRAFT_CONTROL_PORT").ok().and_then(|p| p.parse().ok()),
            files: Vec::new(),
        };
        let mut it = std::env::args_os().skip(1).map(|s| s.to_string_lossy().into_owned()).peekable();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--sample" => a.sample = true,
                "--show" => a.show = true,
                "--control" => match it.next().and_then(|p| p.parse::<std::num::NonZeroU16>().ok()) {
                    Some(p) => a.control = Some(p.get()),
                    None => {
                        let _ = std::io::Write::write_all(&mut std::io::stderr(), b"DeckCraft: --control needs a port number from 1 to 65535\n");
                        std::process::exit(2);
                    }
                },
                "--version" | "-V" => {
                    let _ = std::io::Write::write_fmt(
                        &mut std::io::stdout(),
                        format_args!("DeckCraft {} ({}) for air/OS\n", env!("CARGO_PKG_VERSION"), build_sha()),
                    );
                    std::process::exit(0);
                }
                "--help" | "-h" => {
                    let _ = std::io::Write::write_all(&mut std::io::stdout(), USAGE.as_bytes());
                    std::process::exit(0);
                }
                _ => a.files.push(arg),
            }
        }
        a
    }

    /// The interface thread: builds the app and runs the windows' event loop. `root` is the
    /// first window, the root viewport's.
    fn interface(args: Args, root: i32, scale: f32) {
        let ctx = egui::Context::default();
        deckcraft_ui_egui::install_ime_pass_hooks(&ctx);
        let mut session = Session::new();
        if args.sample
            && let Err(e) = deckcraft_engine::sample::open_sample(&mut session)
        {
            log::warn!("sample: {e}");
        }
        let dialogs = Rc::new(services::Dialogs::default());
        let mut app = SlideApp::new(session, services::services(dialogs.clone()));
        load_prefs(&mut app);
        app.integrated_titlebar = false;
        if std::env::var_os("DECKCRAFT_NO_RECOVERY").is_none() {
            app.session.recovery_dir = deckcraft_engine::recovery::default_dir();
            if let Err(e) = app.session.execute("file.recovery.open", &serde_json::json!({})) {
                log::warn!("recovery: {e}");
            }
        }
        if let Some(port) = args.control {
            app = app.with_control(control_server::start(port, ctx.clone()));
        }
        // Files named on the command line open as File › Open opens them; Tracker's arrive as
        // messages (`shell`). Never the program itself, which a launcher may name.
        let exe = std::env::current_exe().and_then(std::fs::canonicalize).ok();
        // A PowerPoint Show given last starts its slide show.
        let files: Vec<String> = args
            .files
            .into_iter()
            .filter(|f| {
                let me = exe.is_some() && std::fs::canonicalize(f).ok() == exe;
                if me {
                    log::warn!("not opening {f}: it is DeckCraft itself");
                }
                !me
            })
            .collect();
        // `--show` starts the show on what is open, so its files open first; otherwise they open
        // in the background once the window draws, as Tracker's do.
        if args.show {
            for (f, e) in app.open_from_system(&files) {
                log::warn!("{f}: {e}");
            }
            app.start_show(0, false);
        }
        let mut shell = match shell::Shell::new(app, ctx, dialogs, root, scale) {
            Ok(shell) => shell,
            Err(e) => {
                log::error!("{e}");
                host::alert("DeckCraft", &e);
                return;
            }
        };
        if !args.show {
            shell.open_files(files, false);
        }
        shell.run();
        shell.finish();
    }

    pub fn main() {
        // Before any thread starts: the locale variables come from Haiku's Locale preferences.
        host::export_locale();
        let logger = logging::install();
        std::panic::set_hook(Box::new(|info| {
            let trace = std::backtrace::Backtrace::capture();
            let report = if trace.status() == std::backtrace::BacktraceStatus::Captured {
                format!("internal error: {info}\n{trace}")
            } else {
                format!("internal error: {info}")
            };
            if log::log_enabled!(log::Level::Error) {
                log::error!("{report}");
            } else {
                let _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!("DeckCraft: {report}\n"));
            }
        }));
        let args = args();
        // Before the log file, the preferences or the recovery records are touched: a second
        // launch (from Terminal; Tracker hands presentations to the running copy itself) passes
        // its presentations to the running copy and ends.
        match host::init(services::SIGNATURE) {
            Ok(host::Launch::Started) => {}
            Ok(host::Launch::HandedOver) => std::process::exit(0),
            Err(e) => {
                log::error!("{e}");
                std::process::exit(1);
            }
        }
        if let Some(logger) = logger {
            match log_dir().filter(|_| prefs_enabled()) {
                Some(dir) => match logger.attach_dir(&dir) {
                    Ok(path) => log::info!("DeckCraft {} ({}) for air/OS, log file {}", env!("CARGO_PKG_VERSION"), build_sha(), path.display()),
                    Err(e) => log::warn!("no log file: {e}"),
                },
                None => logger.stderr_only(),
            }
        }
        if prefs_enabled() {
            services::claim_document_types();
        }
        // Movies and sounds in formats DeckCraft's own decoders don't read play through the
        // system's codecs.
        media_kit::install();
        // The rasteriser's workers: up to 8 (more cost more in waking than they save on a
        // frame), named for the system's process lists.
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(1, 8));
        if let Err(e) = rayon::ThreadPoolBuilder::new().num_threads(workers).thread_name(|i| format!("DeckCraft raster {i}")).build_global() {
            log::warn!("rasteriser threads: {e}");
        }
        let scale = host::ui_scale();
        let px = |v: f32| v * scale;
        // Where it was last time, fitted onto the monitors there are now; else centred.
        let saved = saved_window();
        let min_size = MIN_SIZE.map(px);
        // The first window opens before the interface starts, so it shows at once.
        let root = host::open_window(&host::WindowSpec {
            title: "DeckCraft".into(),
            position: saved.map(|(at, _)| at),
            size: saved.map_or(DEFAULT_SIZE.map(px), |(_, [w, h])| [w.max(min_size[0]), h.max(min_size[1])]),
            min_size: Some(min_size),
            max_size: None,
            decorations: true,
            resizable: true,
            visible: true,
            zoomed: false,
            fullscreen: false,
            floating: false,
            buttons: [true; 3],
        });
        let Some(root) = root else {
            log::error!("couldn't open the window");
            std::process::exit(1);
        };
        let ui = std::thread::Builder::new().name("DeckCraft interface".into()).stack_size(16 << 20).spawn(move || {
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| interface(args, root, scale)));
            if ran.is_err() {
                log::error!("the interface stopped after an internal error");
                host::alert("DeckCraft", "DeckCraft ran into an internal error and has to close. The log is in ~/config/settings/DeckCraft/logs.");
            }
            host::quit();
        });
        match ui {
            Ok(handle) => {
                host::run();
                let _ = handle.join();
            }
            Err(e) => log::error!("couldn't start the interface: {e}"),
        }
    }
}
