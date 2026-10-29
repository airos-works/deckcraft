//! The interface thread's event loop: Haiku's input goes to the window it happened in
//! (`viewports`, which gives every egui viewport a window of its own: the editor's, View › New
//! Window's and the slide show's presenter view), SlideApp runs its frame in the root viewport's
//! pass, and egui's output goes back to the windows and the system.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deckcraft_ui_egui::{InboxEvent, InboxFile, SlideApp};
use egui::{ViewportCommand, ViewportId};

use crate::host::{self, RawEvent, kind};
use crate::platform::{Haiku, copy_picture};
use crate::services::{Dialogs, OBJECTS_MARKER};
use crate::viewports::{Viewports, WindowInput};
use crate::{save_prefs, save_window};

pub struct Shell {
    pub app: SlideApp,
    ctx: egui::Context,
    viewports: Viewports<Haiku>,
    /// The first window, the root viewport's: the editor and the slide show.
    root: i32,
    dialogs: Rc<Dialogs>,
    /// The interface scale (the system font against 12 pt) and device pixels per logical pixel
    /// ([`host::device_scale`]), and when the latter was last asked.
    ui_scale: f32,
    dev: f32,
    dev_checked: Instant,
    quit: bool,
    /// Files were opened from Tracker or a later launch: the next frame brings the window forward.
    activate: bool,
    /// The editor copied shapes without text ([`Haiku::copied_shapes`]).
    copied_shapes: Rc<Cell<bool>>,
    /// Files saved (or about to be) that need their MIME type, until when to keep trying.
    to_type: Vec<(PathBuf, Instant)>,
    /// Each presentation's last saved state (its uid, path and saved copy), to notice a save.
    last_saved: Vec<(u64, Option<String>, usize)>,
    /// Presentations waiting to be opened in the background, one after the other.
    to_open: VecDeque<String>,
    /// The window's frame corner and content size while it was last an ordinary window (not
    /// minimized, not a slide show), saved for the next run.
    window_frame: Option<[f32; 4]>,
}

impl Shell {
    /// The shell around `app`, whose first window (the root viewport's) is native window `root`.
    pub fn new(app: SlideApp, ctx: egui::Context, dialogs: Rc<Dialogs>, root: i32, ui_scale: f32) -> Result<Self, String> {
        let dev = host::device_scale();
        let platform = Haiku::new(root)?;
        let copied_shapes = platform.copied_shapes.clone();
        let viewports = Viewports::new(&ctx, platform, root, ui_scale * dev, dev);
        Ok(Self {
            app,
            ctx,
            viewports,
            root,
            dialogs,
            ui_scale,
            dev,
            dev_checked: Instant::now(),
            quit: false,
            activate: false,
            copied_shapes,
            to_type: Vec::new(),
            last_saved: Vec::new(),
            to_open: VecDeque::new(),
            window_frame: None,
        })
    }

    /// Runs until the app quits.
    pub fn run(&mut self) {
        // A print job still being sent to the printer finishes first (its worker would die with
        // the process); the windows keep drawing until then.
        while !self.quit || self.app.printing_busy() {
            host::wait(self.viewports.timeout());
            self.drain();
            if self.dev_checked.elapsed() > Duration::from_secs(2) {
                self.check_density();
            }
            for viewport in self.viewports.take_due() {
                if viewport == ViewportId::ROOT {
                    self.frame();
                } else if self.viewports.run_deferred(viewport).close_root {
                    self.quit = true;
                }
            }
            self.viewports.chores();
        }
    }

    /// Asks for the desktop's density again (the monitors may have changed).
    fn check_density(&mut self) {
        self.dev_checked = Instant::now();
        let dev = host::device_scale();
        if (dev - self.dev).abs() > f32::EPSILON {
            log::info!("display density {} -> {dev}", self.dev);
            self.dev = dev;
            self.viewports.set_density(self.ui_scale * dev, dev);
        }
    }

    /// Hands the queued native events to their windows, starting with those that came while a
    /// file panel was open; the application's own are handled here.
    fn drain(&mut self) {
        loop {
            let deferred = self.dialogs.deferred.borrow_mut().pop_front();
            let Some((ev, text)) = deferred.or_else(host::next_event) else { break };
            if let Some(input) = window_input(&ev, &text) {
                if !self.viewports.input(ev.window, input) {
                    log::debug!("event {} for window {}, which is gone", ev.kind, ev.window);
                }
                continue;
            }
            match ev.kind {
                // Quit (Command+Q, the Deskbar's Quit) closes the editor's window, which asks
                // about unsaved presentations first.
                kind::QUIT_REQUESTED => {
                    self.viewports.input(self.root, WindowInput::Close);
                }
                // Tracker's Open, Open With and drops on the icon, a later launch's files (one
                // copy runs: B_SINGLE_LAUNCH), and files dropped on a window.
                kind::OPEN_FILES | kind::DROPPED_FILES => {
                    let paths: Vec<String> = text.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
                    if ev.kind == kind::OPEN_FILES && ev.id == 1 {
                        self.show_files(&paths);
                    } else {
                        self.open_files(paths, ev.kind == kind::DROPPED_FILES);
                    }
                    self.activate = ev.kind == kind::OPEN_FILES;
                    self.viewports.request(ViewportId::ROOT, Duration::ZERO);
                }
                kind::COLORS => {
                    self.check_density();
                    self.viewports.request(ViewportId::ROOT, Duration::ZERO);
                }
                // A panel's answer after its caller stopped waiting has nobody to go to.
                _ => {}
            }
        }
    }

    /// A later launch's `--show`, as at start-up (`main`): its presentations open at once and the
    /// slide show starts on the one shown (with none, on the presentation already shown).
    fn show_files(&mut self, paths: &[String]) {
        for (f, e) in self.app.open_from_system(paths) {
            self.app.set_status(format!("{f}: {e}"));
        }
        self.app.start_show(0, false);
    }

    /// Files the system hands over, without holding up the window: presentations are read and
    /// parsed in the background behind the editor's progress window (File › Open's way), one
    /// after the other, each replacing an untouched blank presentation. Tracker's other files
    /// (outlines) open as the desktop app opens them, and a PowerPoint Show handed over last
    /// starts its slide show at once. Files `dropped` on the window open or are inserted as the
    /// desktop app's drops are (a picture, sound or movie onto the slide shown when it was
    /// dropped), read on a thread and delivered through the editor's inbox.
    pub fn open_files(&mut self, paths: Vec<String>, dropped: bool) {
        if !dropped && paths.last().is_some_and(|p| p.to_ascii_lowercase().ends_with(".ppsx")) {
            for (_, e) in self.app.open_from_system(&paths) {
                self.app.set_status(e);
            }
            return;
        }
        for path in paths {
            if is_presentation(&path) {
                self.to_open.push_back(path);
            } else if dropped {
                self.read_dropped(path);
            } else {
                for (_, e) in self.app.open_from_system(std::slice::from_ref(&path)) {
                    self.app.set_status(e);
                }
            }
        }
        self.open_next();
    }

    /// The next waiting presentation, once the one before it has opened.
    fn open_next(&mut self) {
        if self.app.opening.is_none()
            && let Some(path) = self.to_open.pop_front()
        {
            self.app.open_path_in_background(&path);
            self.viewports.request(ViewportId::ROOT, Duration::ZERO);
        }
    }

    /// A dropped file for the slide shown now, read on a thread into the editor's inbox.
    fn read_dropped(&mut self, path: String) {
        let Some(inbox) = self.app.services.inbox.clone() else { return };
        let id = self.app.begin_external_file("drop");
        let name = std::path::Path::new(&path).file_name().map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
        let ctx = self.ctx.clone();
        let read = move || {
            let event = match std::fs::read(&path) {
                Ok(bytes) => InboxEvent::File(InboxFile { id, name, bytes }),
                Err(e) => {
                    log::warn!("{path}: {e}");
                    InboxEvent::Cancelled { id }
                }
            };
            match inbox.lock() {
                Ok(mut events) => events.push(event),
                Err(poisoned) => poisoned.into_inner().push(event),
            }
            ctx.request_repaint();
        };
        if let Err(e) = std::thread::Builder::new().name("DeckCraft read".into()).spawn(read) {
            log::warn!("couldn't read the dropped file: {e}");
        }
    }

    /// The desktop app's per-frame logic (`apps/deckcraft`): the app's own, the next waiting
    /// presentation, and the slide show's full screen.
    fn logic(&mut self, ctx: &egui::Context) {
        self.app.logic(ctx);
        self.open_next();
        // A native close during a show opens Save Changes in the editor. The UI can clear
        // `show` without emitting Fullscreen(false), so restore its native frame as well.
        if self.app.show.is_none() && host::window_info(self.root).fullscreen {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        }
        if std::mem::take(&mut self.activate) {
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
    }

    /// The root viewport's pass: the app's frame, which draws the other windows (View › New
    /// Window, the presenter view) in their own viewports on the way.
    fn frame(&mut self) {
        let window = host::window_info(self.root);
        if !window.fullscreen && !window.minimized && window.inner[2] > window.inner[0] {
            let [left, top, ..] = window.outer;
            let [l, t, r, b] = window.inner;
            self.window_frame = Some([left, top, r - l, b - t]);
        }
        let Some(mut raw) = self.viewports.root_input() else { return };
        let close = raw.viewports.get(&ViewportId::ROOT).is_some_and(|i| i.close_requested());
        self.app.raw_input_hook(&self.ctx, &mut raw);
        let ctx = self.ctx.clone();
        let t0 = Instant::now();
        let out = ctx.run_ui(raw, |ui| {
            let ctx = ui.ctx().clone();
            self.logic(&ctx);
            self.app.ui(ui);
        });
        let t1 = Instant::now();
        self.type_saved_files();
        // Honor the editor's unsaved-changes prompt before accepting a native close.
        let cancelled = out.viewport_output.get(&ViewportId::ROOT).is_some_and(|vp| vp.commands.iter().any(|c| matches!(c, ViewportCommand::CancelClose)));
        let closed = self.viewports.finish(ViewportId::ROOT, out).close_root;
        if (close || closed) && !cancelled {
            self.quit = true;
        }
        if self.copied_shapes.take() {
            // Copied shapes without text (pictures, drawings) leave the editor's marker as their
            // text; other programs also get a picture of them.
            let copied = deckcraft_engine::cmd::edit::copied_shapes_png(&self.app.session);
            log::debug!("copy: picture of the shapes {:?}", copied.as_ref().map(Vec::len));
            if !copied.is_some_and(|png| copy_picture(Some(OBJECTS_MARKER), png)) && !host::clipboard_set(OBJECTS_MARKER, None) {
                log::warn!("clipboard: couldn't copy");
            }
        }
        log::debug!("frame: interface {:.1} ms, output and drawing {:.1} ms", (t1 - t0).as_secs_f64() * 1e3, t1.elapsed().as_secs_f64() * 1e3);
        if !self.to_type.is_empty() {
            // Look again shortly for the files still being written.
            self.viewports.request(ViewportId::ROOT, Duration::from_millis(250));
        }
    }

    /// Files DeckCraft saves are written by the engine with plain file calls; Haiku programs'
    /// files carry their MIME type, which Tracker opens them by. A presentation whose saved copy
    /// changed with a path was just saved; a Save panel's path (an export too) may be written a
    /// frame or two later.
    fn type_saved_files(&mut self) {
        let now = Instant::now();
        let state: Vec<(u64, Option<String>, usize)> =
            self.app.session.documents().iter().map(|d| (d.uid, d.path.clone(), Arc::as_ptr(&d.saved_doc) as usize)).collect();
        for (uid, path, saved) in &state {
            let before = self.last_saved.iter().find(|(u, ..)| u == uid);
            if let Some(path) = path
                && before.is_some_and(|(_, p, s)| s != saved || p.as_ref() != Some(path))
            {
                self.to_type.push((PathBuf::from(path), now + Duration::from_secs(10)));
            }
        }
        self.last_saved = state;
        for path in self.dialogs.saved.borrow_mut().drain(..) {
            self.to_type.push((path, now + Duration::from_secs(10)));
        }
        self.to_type.retain(|(path, until)| {
            if path.exists() {
                if !host::type_file(path) {
                    log::debug!("couldn't set the type of {}", path.display());
                }
                false
            } else {
                now < *until
            }
        });
    }

    /// What quitting does besides closing the window (`on_exit` in the desktop app).
    pub fn finish(&mut self) {
        save_prefs(&self.app);
        if let Some(frame) = self.window_frame {
            save_window(frame);
        }
    }
}

/// A native event that happened in a window, as that window's input. Files dropped on a window
/// are the shell's (they open or are inserted on the slide shown).
fn window_input<'a>(ev: &RawEvent, text: &'a str) -> Option<WindowInput<'a>> {
    Some(match ev.kind {
        kind::MOUSE_MOVED | kind::MOUSE_DOWN | kind::MOUSE_UP => {
            WindowInput::Pointer { x: ev.x, y: ev.y, buttons: ev.buttons, modifiers: ev.modifiers }
        }
        kind::POINTER_GONE => WindowInput::PointerGone,
        // A wheel's whole steps, or (`repeat` 1) a touchpad's scroll gesture with its unrounded
        // steps and its phase in `clicks` (`host.cpp`).
        kind::WHEEL => WindowInput::Wheel { dx: ev.dx, dy: ev.dy, modifiers: ev.modifiers, gesture: (ev.repeat != 0).then_some(ev.clicks) },
        kind::KEY_DOWN | kind::KEY_UP => {
            WindowInput::Key { pressed: ev.kind == kind::KEY_DOWN, code: ev.key, raw_char: ev.raw_char, modifiers: ev.modifiers, text }
        }
        kind::MODIFIERS => WindowInput::Modifiers(host::modifiers()),
        kind::FOCUS => WindowInput::Focus(ev.x != 0.0),
        kind::CLOSE => WindowInput::Close,
        // The next frame picks up the new size, position or state.
        kind::RESIZED | kind::MOVED | kind::MINIMIZED => WindowInput::Changed,
        // An input method's composition: `id` says what, the pre-edit's selected clause is
        // bytes `key` to `raw_char` (-1: none).
        kind::IME => match ev.id {
            host::ime::PREEDIT => WindowInput::Preedit { text, selection: (ev.key >= 0 && ev.raw_char >= 0).then_some((ev.key, ev.raw_char)) },
            host::ime::COMMIT => WindowInput::Commit(text),
            host::ime::STOPPED => WindowInput::ImeStopped,
            _ => return None,
        },
        _ => return None,
    })
}

/// A presentation by its name, as the editor tells one from other files it opens.
fn is_presentation(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".deckcraft", ".slidecraft", ".pptx", ".potx", ".ppsx"].iter().any(|e| lower.ends_with(e))
}

