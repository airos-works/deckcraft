//! The window's event loop on the interface thread: Haiku's input becomes egui's, SlideApp runs
//! its frame, and egui's output goes back to the window and the system (title, cursor,
//! clipboard, links) and, through Vulkan (`gpu`) or the software rasteriser, to the screen.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deckcraft_ui_egui::{InboxEvent, InboxFile, SlideApp};
use egui::{Event, Key, Modifiers, PointerButton, Pos2, Theme, ViewportCommand, ViewportEvent, ViewportId, ViewportInfo, pos2, vec2};

use crate::host::{self, RawEvent, WindowCommand, kind, wheel_phase};
use crate::raster::Rasterizer;
use crate::services::{self, Dialogs, OBJECTS_MARKER};
use crate::{keys, save_prefs, save_window};

pub struct Shell {
    pub app: SlideApp,
    ctx: egui::Context,
    raster: Rasterizer,
    /// The GPU renderer; `None` draws with `raster` (no hardware Vulkan adapter,
    /// `DECKCRAFT_RENDERER=cpu`, or the device was lost). The rasteriser keeps egui's textures
    /// either way, so it can take over between two frames.
    gpu: Option<crate::gpu::Gpu>,
    dialogs: Rc<Dialogs>,
    start: Instant,
    /// egui's points to device pixels, as of the last frame, and the system's own: the interface
    /// scale (the system font against 12 pt) times the desktop's density.
    ppp: f32,
    native_ppp: f32,
    ui_scale: f32,
    /// Device pixels per logical pixel ([`host::device_scale`]), and when it was last asked.
    dev: f32,
    dev_checked: Instant,
    buttons: i32,
    modifiers: Modifiers,
    focused: bool,
    events: Vec<Event>,
    /// A touchpad gesture's end held back for the next frame (its modifiers), see [`Self::wheel`].
    wheel_end: Option<Modifiers>,
    close_requested: bool,
    quit: bool,
    /// When egui wants the next frame (from this thread's frames).
    repaint_at: Option<Instant>,
    /// When other threads asked for one (`request_repaint` from a worker).
    woken_for: Arc<Mutex<Option<Instant>>>,
    screenshots: Vec<egui::ScreenshotCallback>,
    title: String,
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

/// egui's cursor as one of Haiku's `BCursorID`s.
fn cursor_id(icon: egui::CursorIcon) -> i32 {
    use egui::CursorIcon as C;
    match icon {
        C::None => 12,
        C::ContextMenu => 3,
        C::Help => 9,
        C::PointingHand => 6,
        C::Progress | C::Wait => 14,
        C::Cell | C::Crosshair => 5,
        C::Text => 2,
        C::VerticalText => 10,
        C::Alias => 29,
        C::Copy => 4,
        C::Move | C::AllScroll => 11,
        C::NoDrop | C::NotAllowed => 13,
        C::Grab => 7,
        C::Grabbing => 8,
        C::ResizeHorizontal | C::ResizeColumn => 24,
        C::ResizeVertical | C::ResizeRow => 23,
        C::ResizeNeSw => 25,
        C::ResizeNwSe => 26,
        C::ResizeEast => 16,
        C::ResizeSouthEast => 21,
        C::ResizeSouth => 17,
        C::ResizeSouthWest => 22,
        C::ResizeWest => 18,
        C::ResizeNorthWest => 20,
        C::ResizeNorth => 15,
        C::ResizeNorthEast => 19,
        C::ZoomIn => 27,
        C::ZoomOut => 28,
        _ => 1,
    }
}

fn button(bit: i32) -> Option<PointerButton> {
    Some(match bit {
        1 => PointerButton::Primary,
        2 => PointerButton::Secondary,
        4 => PointerButton::Middle,
        8 => PointerButton::Extra1,
        16 => PointerButton::Extra2,
        _ => return None,
    })
}

/// Runs `f`; a panic inside it (a wgpu validation failure, a driver gone wrong) becomes an
/// error, so the window falls back to the rasteriser instead of losing the open documents.
fn guard<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| Err(format!("{what} failed with an internal error (logged)")))
}

/// The GPU renderer `DECKCRAFT_RENDERER` asks for: `auto` (the default: Vulkan on a hardware
/// adapter, else the rasteriser), `vulkan` (Vulkan or an error) or `cpu`.
fn start_gpu() -> Result<Option<crate::gpu::Gpu>, String> {
    let mode = std::env::var("DECKCRAFT_RENDERER").unwrap_or_else(|_| "auto".into());
    let gpu = match mode.as_str() {
        "cpu" => None,
        "auto" | "vulkan" => match guard("GPU startup", crate::gpu::Gpu::new) {
            Ok(gpu) => Some(gpu),
            Err(e) if mode == "vulkan" => return Err(e),
            Err(e) => {
                log::warn!("GPU startup: {e}; using the software rasteriser");
                None
            }
        },
        _ => return Err("DECKCRAFT_RENDERER must be auto, cpu or vulkan".into()),
    };
    log::info!("air/OS renderer: {}", if gpu.is_some() { "Vulkan" } else { "CPU" });
    Ok(gpu)
}

impl Shell {
    pub fn new(app: SlideApp, ctx: egui::Context, dialogs: Rc<Dialogs>, ui_scale: f32) -> Result<Self, String> {
        let gpu = start_gpu()?;
        let dev = host::device_scale();
        let native_ppp = ui_scale * dev;
        let woken_for: Arc<Mutex<Option<Instant>>> = Arc::default();
        let wake = woken_for.clone();
        ctx.set_request_repaint_callback(move |info| {
            let at = Instant::now() + info.delay;
            if let Ok(mut w) = wake.lock() {
                *w = Some(w.map_or(at, |old| old.min(at)));
            }
            host::wake();
        });
        Ok(Self {
            app,
            ctx,
            raster: Rasterizer::new(),
            gpu,
            dialogs,
            start: Instant::now(),
            ppp: native_ppp,
            native_ppp,
            ui_scale,
            dev,
            dev_checked: Instant::now(),
            buttons: 0,
            modifiers: Modifiers::default(),
            focused: true,
            events: Vec::new(),
            wheel_end: None,
            close_requested: false,
            quit: false,
            repaint_at: Some(Instant::now()),
            woken_for,
            screenshots: Vec::new(),
            title: String::new(),
            to_type: Vec::new(),
            last_saved: Vec::new(),
            to_open: VecDeque::new(),
            window_frame: None,
        })
    }

    /// Runs until the app quits.
    pub fn run(&mut self) {
        while !self.quit {
            host::wait(self.timeout());
            let happened = self.drain();
            let due = self.take_due();
            if happened || due {
                self.frame();
            }
            self.present_chores();
        }
    }

    /// What the presenter does between frames: the last frame again where the window moved on
    /// the screen, and app_server's copy catching up with frames that went straight to the screen.
    fn present_chores(&mut self) {
        let dev = self.dev;
        let Some(gpu) = &mut self.gpu else { return };
        let result = if gpu.wants_present() {
            guard("GPU present", || gpu.present_again(dev))
        } else if gpu.sync_due().is_some_and(|at| at <= Instant::now()) {
            guard("GPU sync", || gpu.sync(dev))
        } else {
            return;
        };
        if let Err(e) = result {
            self.drop_gpu(&e);
        }
    }

    /// Back to the software rasteriser for good: the device failed (`why`). The next frame
    /// draws everything with it.
    fn drop_gpu(&mut self, why: &str) {
        if self.gpu.take().is_some() {
            log::error!("GPU: {why}; switching to the software rasteriser");
            self.raster.invalidate();
            self.repaint_at = Some(Instant::now());
        }
    }

    /// When the next frame is due, if one is.
    fn next_due(&self) -> Option<Instant> {
        let woken = self.woken_for.lock().ok().and_then(|w| *w);
        let sync = self.gpu.as_ref().and_then(|g| g.sync_due());
        [self.repaint_at, woken, sync].into_iter().flatten().min()
    }

    fn timeout(&self) -> Option<Duration> {
        self.next_due().map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// Whether a frame is due now; clears what made it due.
    fn take_due(&mut self) -> bool {
        let now = Instant::now();
        let due = self.next_due().is_some_and(|at| at <= now);
        if due {
            self.repaint_at = None;
            if let Ok(mut w) = self.woken_for.lock()
                && w.is_some_and(|at| at <= now)
            {
                *w = None;
            }
        }
        due
    }

    /// A point in the window (logical pixels) in egui's points.
    fn pos(&self, x: f32, y: f32) -> Pos2 {
        pos2(x * self.dev / self.ppp, y * self.dev / self.ppp)
    }

    /// Asks for the desktop's density again (the monitors may have changed).
    fn check_density(&mut self) {
        self.dev_checked = Instant::now();
        let dev = host::device_scale();
        if (dev - self.dev).abs() > f32::EPSILON {
            log::info!("display density {} -> {dev}", self.dev);
            self.dev = dev;
            self.native_ppp = self.ui_scale * dev;
        }
    }

    fn set_modifiers(&mut self, bits: u32) {
        let m = keys::modifiers(bits);
        if m != self.modifiers {
            self.modifiers = m;
            self.events.push(Event::ModifiersChanged(m));
        }
    }

    /// Turns the queued native events into egui's, starting with those that came while a file
    /// panel was open. Whether there were any.
    fn drain(&mut self) -> bool {
        let mut any = false;
        if let Some(modifiers) = self.wheel_end.take() {
            any = true;
            self.events.push(Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::Vec2::ZERO,
                phase: egui::TouchPhase::End,
                source: egui::MouseWheelSource::Trackpad,
                modifiers,
            });
        }
        loop {
            let deferred = self.dialogs.deferred.borrow_mut().pop_front();
            let Some((ev, text)) = deferred.or_else(host::next_event) else { break };
            any = true;
            self.event(ev, text);
        }
        any
    }

    fn event(&mut self, ev: RawEvent, text: String) {
        match ev.kind {
            kind::MOUSE_MOVED | kind::MOUSE_DOWN | kind::MOUSE_UP => {
                self.set_modifiers(ev.modifiers);
                let pos = self.pos(ev.x, ev.y);
                self.events.push(Event::PointerMoved(pos));
                let changed = ev.buttons ^ self.buttons;
                for bit in [1, 2, 4, 8, 16] {
                    if changed & bit != 0
                        && let Some(b) = button(bit)
                    {
                        self.events.push(Event::PointerButton { pos, button: b, pressed: ev.buttons & bit != 0, modifiers: self.modifiers });
                    }
                }
                self.buttons = ev.buttons;
            }
            kind::POINTER_GONE => self.events.push(Event::PointerGone),
            kind::WHEEL => {
                self.set_modifiers(ev.modifiers);
                self.wheel(&ev);
            }
            kind::KEY_DOWN | kind::KEY_UP => self.key(ev.kind == kind::KEY_DOWN, ev.key, ev.raw_char, ev.modifiers, &text),
            kind::MODIFIERS => self.set_modifiers(host::modifiers()),
            kind::FOCUS => {
                self.focused = ev.x != 0.0;
                self.events.push(Event::WindowFocused(self.focused));
                if !self.focused {
                    // Keys released elsewhere never come back up.
                    self.set_modifiers(0);
                }
            }
            kind::CLOSE | kind::QUIT_REQUESTED => self.close_requested = true,
            // Tracker's Open, Open With and drops on the icon, and files dropped on the window.
            kind::OPEN_FILES | kind::DROPPED_FILES => {
                let paths: Vec<String> = text.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
                self.open_files(paths, ev.kind == kind::DROPPED_FILES);
                host::window_command(WindowCommand::Activate);
            }
            kind::COLORS => self.check_density(),
            // The next frame picks up the new size, position or state.
            kind::RESIZED | kind::MOVED | kind::MINIMIZED => {}
            // A panel's answer after its caller stopped waiting has nobody to go to.
            _ => {}
        }
    }

    /// A wheel turn, or a step of an air/OS touchpad's scroll gesture: its precise deltas (wheel
    /// steps, unrounded) become points at egui's line scroll speed, with the gesture's phases, so
    /// egui and the editor treat it as a trackpad (the canvas scrolls with the fingers when zoomed
    /// in and a flick turns the slide, with the trackpad's cooldown). A wheel's whole steps stay
    /// lines, one slide each.
    fn wheel(&mut self, ev: &RawEvent) {
        use egui::{MouseWheelSource, MouseWheelUnit, TouchPhase};
        let modifiers = self.modifiers;
        // `repeat`: precise deltas (`be:wheel_precise_delta_*`), `clicks`: `be:wheel_phase`.
        if ev.repeat == 0 {
            // A mouse wheel (B_MOUSE_POINTING_DEVICE), or a system without touchpad gestures.
            self.events.push(Event::MouseWheel {
                unit: MouseWheelUnit::Line,
                delta: vec2(-ev.dx, -ev.dy),
                phase: TouchPhase::Move,
                source: MouseWheelSource::Wheel,
                modifiers,
            });
            return;
        }
        let event = |unit, delta, phase| Event::MouseWheel { unit, delta, phase, source: MouseWheelSource::Trackpad, modifiers };
        let line = self.ctx.options(|o| o.input_options.line_scroll_speed);
        let delta = vec2(-ev.dx, -ev.dy) * line;
        let delta = if delta.is_finite() { delta } else { egui::Vec2::ZERO };
        match ev.clicks {
            wheel_phase::BEGAN => {
                self.events.push(event(MouseWheelUnit::Point, egui::Vec2::ZERO, TouchPhase::Start));
            }
            wheel_phase::ENDED => {
                // egui drops a gesture's unapplied deltas at its end: one that ends in the same
                // frame as its last movement ends in the next.
                let moved = self.events.iter().any(|e| matches!(e, Event::MouseWheel { source: MouseWheelSource::Trackpad, phase: TouchPhase::Move, .. }));
                if moved {
                    self.wheel_end = Some(modifiers);
                } else {
                    self.events.push(event(MouseWheelUnit::Point, egui::Vec2::ZERO, TouchPhase::End));
                }
                return;
            }
            _ => {}
        }
        if delta != egui::Vec2::ZERO {
            self.events.push(event(MouseWheelUnit::Point, delta, TouchPhase::Move));
        }
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
            self.repaint_at = Some(Instant::now());
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

    fn key(&mut self, pressed: bool, code: i32, raw_char: i32, bits: u32, text: &str) {
        self.set_modifiers(bits);
        let (logical, physical) = keys::key(code, raw_char, text.as_bytes());
        log::debug!(
            "key {} code {code:#x} raw {raw_char:#x} modifiers {bits:#x} {text:?}: {logical:?} / {physical:?}",
            if pressed { "down" } else { "up" }
        );
        let m = self.modifiers;
        if let Some(active) = logical.or(physical) {
            if pressed {
                // As egui's own integrations do: the clipboard shortcuts become clipboard events.
                if m.command && active == Key::X || m.shift && active == Key::Delete {
                    self.events.push(Event::Cut);
                    return;
                }
                if m.command && (active == Key::C || active == Key::Insert) {
                    self.events.push(Event::Copy);
                    return;
                }
                if m.command && active == Key::V || m.shift && active == Key::Insert {
                    self.paste();
                    return;
                }
            }
            self.events.push(Event::Key { key: active, physical_key: physical, pressed, repeat: false, modifiers: m });
        }
        if pressed && let Some(t) = keys::text(bits, text) {
            self.events.push(Event::Text(t.to_string()));
        }
    }

    /// Paste: the clipboard's text, or, without text, an empty paste, which the editor answers
    /// with the clipboard's picture (`Services::clipboard_image`).
    fn paste(&mut self) {
        match services::clipboard_text() {
            Some(t) => self.events.push(Event::Paste(t.replace("\r\n", "\n"))),
            None if services::clipboard_has_picture() => self.events.push(Event::Paste(String::new())),
            None => {}
        }
    }

    fn viewport_info(&self, close: bool) -> ViewportInfo {
        let w = host::window_info();
        let k = self.dev / self.ppp;
        let rect = |r: [f32; 4]| egui::Rect::from_min_max(pos2(r[0], r[1]) * k, pos2(r[2], r[3]) * k);
        ViewportInfo {
            title: Some(self.title.clone()),
            events: if close { vec![ViewportEvent::Close] } else { Vec::new() },
            native_pixels_per_point: Some(self.native_ppp),
            monitor_size: (w.screen[0] > 0.0).then(|| vec2(w.screen[0], w.screen[1]) * k),
            inner_rect: Some(rect(w.inner)),
            outer_rect: Some(rect(w.outer)),
            minimized: Some(w.minimized),
            maximized: Some(false),
            fullscreen: Some(w.fullscreen),
            focused: Some(w.active),
            ..Default::default()
        }
    }

    /// The desktop app's per-frame logic (`apps/deckcraft`): the app's own, and the window's
    /// close button, which waits for a print job being handed over.
    fn logic(&mut self, ctx: &egui::Context) {
        self.app.logic(ctx);
        self.open_next();
    }

    fn frame(&mut self) {
        if self.gpu.as_ref().is_some_and(|g| g.faulted()) {
            self.drop_gpu("the device was lost or failed");
        }
        if self.dev_checked.elapsed() > Duration::from_secs(2) {
            self.check_density();
        }
        let (lw, lh) = host::view_size();
        let (w, h) = ((lw * self.dev).round(), (lh * self.dev).round());
        if w < 1.0 || h < 1.0 {
            self.repaint_at = Some(Instant::now() + Duration::from_millis(50));
            return;
        }
        let close = std::mem::take(&mut self.close_requested);
        let window = host::window_info();
        if !window.fullscreen && !window.minimized {
            let [left, top, ..] = window.outer;
            let [l, t, r, b] = window.inner;
            self.window_frame = Some([left, top, r - l, b - t]);
        }
        let raw = egui::RawInput {
            viewport_id: ViewportId::ROOT,
            viewports: std::iter::once((ViewportId::ROOT, self.viewport_info(close))).collect(),
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, vec2(w, h) / self.ppp)),
            max_texture_side: Some(self.gpu.as_ref().map_or(8192, |g| g.max_texture_side().min(16384))),
            time: Some(self.start.elapsed().as_secs_f64()),
            predicted_dt: 1.0 / 60.0,
            events: std::mem::take(&mut self.events),
            focused: self.focused,
            system_theme: Some(if host::dark() { Theme::Dark } else { Theme::Light }),
            ..Default::default()
        };
        let mut raw = raw;
        self.app.raw_input_hook(&self.ctx, &mut raw);
        let ctx = self.ctx.clone();
        let t0 = Instant::now();
        let mut out = ctx.run_ui(raw, |ui| {
            let ctx = ui.ctx().clone();
            self.logic(&ctx);
            self.app.ui(ui);
        });

        // A native close during a show opens Save Changes in the editor. The UI can clear
        // `show` without emitting Fullscreen(false), so restore its native frame as well.
        if self.app.show.is_none() && host::window_info().fullscreen {
            host::window_command(WindowCommand::Fullscreen(false));
        }
        self.type_saved_files();

        // The system side of egui's output.
        host::set_cursor(cursor_id(out.platform_output.cursor_icon));
        for cmd in &out.platform_output.commands {
            log::debug!("output {cmd:?}");
            match cmd {
                egui::OutputCommand::CopyText(t) => {
                    // Copied shapes without text (pictures, drawings) leave the editor's marker
                    // as their text; other programs also get a picture of them.
                    let copied = (t == OBJECTS_MARKER).then(|| deckcraft_engine::cmd::edit::copied_shapes_png(&self.app.session)).flatten();
                    log::debug!("copy: picture of the shapes {:?}", copied.as_ref().map(Vec::len));
                    if !copied.is_some_and(|png| copy_picture(Some(t), png)) && !host::clipboard_set(t, None) {
                        log::warn!("clipboard: couldn't copy");
                    }
                }
                egui::OutputCommand::CopyImage(img) => copy_image(img),
                egui::OutputCommand::OpenUrl(u) => {
                    if !host::open_url(&u.url) {
                        log::warn!("couldn't open {}", u.url);
                    }
                }
                // Haiku has no PRIMARY selection to put it in.
                egui::OutputCommand::TextSelectionSettled(_) => {}
            }
        }
        let mut delay = Duration::MAX;
        if let Some(vp) = out.viewport_output.get(&ViewportId::ROOT) {
            delay = vp.repaint_delay;
            if let Some(t) = &vp.builder.title
                && *t != self.title
            {
                self.title.clone_from(t);
                host::set_title(t);
            }
            // Honor the editor's unsaved-changes prompt before accepting a native close.
            let requested = close || vp.commands.iter().any(|c| matches!(c, ViewportCommand::Close));
            if requested && !vp.commands.iter().any(|c| matches!(c, ViewportCommand::CancelClose)) {
                self.quit = true;
            }
            for cmd in &vp.commands {
                self.command(cmd);
            }
        }

        // Draw.
        let ppp = out.pixels_per_point;
        self.raster.set_textures(&out.textures_delta);
        let minimized = host::window_info().minimized;
        if !minimized {
            let t1 = Instant::now();
            let prims = ctx.tessellate(out.shapes, ppp);
            let t2 = Instant::now();
            let clear = ctx.global_style().visuals.panel_fill;
            let (pw, ph) = (w as usize, h as usize);
            let dev = self.dev;
            // The presenter shows GPU frames itself; the rasteriser's are shown below.
            let mut failed = None;
            let dirty = match &mut self.gpu {
                Some(gpu) => {
                    let size = [u32::try_from(pw).unwrap_or(u32::MAX), u32::try_from(ph).unwrap_or(u32::MAX)];
                    if let Err(e) = guard("GPU frame", || gpu.render(size, &prims, &out.textures_delta, ppp, clear, dev)) {
                        failed = Some(e);
                    }
                    None
                }
                None => self.raster.render([pw, ph], &prims, ppp, clear),
            };
            if let Some(e) = failed {
                self.drop_gpu(&e);
            }
            let t3 = Instant::now();
            if let Some(dirty) = dirty {
                host::present(self.raster.pixels(), pw, ph, dirty, self.dev);
            }
            log::debug!(
                "frame {pw}x{ph}: ui {:.1} ms, tessellate {:.1} ms, raster {:.1} ms, present {:.1} ms, dirty {dirty:?}",
                (t1 - t0).as_secs_f64() * 1e3,
                (t2 - t1).as_secs_f64() * 1e3,
                (t3 - t2).as_secs_f64() * 1e3,
                t3.elapsed().as_secs_f64() * 1e3
            );
        }
        self.raster.free_textures(&out.textures_delta);
        // Applied: epaint checks (in debug builds) that no texture update is dropped unhandled.
        out.textures_delta.clear();
        for callback in std::mem::take(&mut self.screenshots) {
            let image = match self.gpu.as_mut() {
                Some(gpu) => gpu.image().unwrap_or_else(|e| {
                    log::warn!("screenshot: {e}");
                    egui::ColorImage::default()
                }),
                None => self.raster.image(),
            };
            callback.complete(Arc::new(image));
            delay = Duration::ZERO;
        }
        if (ppp - self.ppp).abs() > f32::EPSILON {
            self.ppp = ppp;
            delay = Duration::ZERO;
        }
        if !self.events.is_empty() || self.wheel_end.is_some() {
            delay = Duration::ZERO;
        }
        if !self.to_type.is_empty() {
            // Look again shortly for the files still being written.
            delay = delay.min(Duration::from_millis(250));
        }
        self.repaint_at = (delay < Duration::from_secs(3600)).then(|| Instant::now() + delay);
    }

    fn command(&mut self, cmd: &ViewportCommand) {
        // Points to logical pixels.
        let ppp = self.ppp / self.dev;
        match cmd {
            ViewportCommand::Title(t) => {
                if *t != self.title {
                    self.title.clone_from(t);
                    host::set_title(t);
                }
            }
            ViewportCommand::InnerSize(s) => host::window_command(WindowCommand::ResizeTo(s.x * ppp, s.y * ppp)),
            ViewportCommand::MinInnerSize(s) => host::window_command(WindowCommand::MinSize(s.x * ppp, s.y * ppp)),
            ViewportCommand::OuterPosition(p) => host::window_command(WindowCommand::MoveTo(p.x * ppp, p.y * ppp)),
            ViewportCommand::Minimized(m) => host::window_command(WindowCommand::Minimize(*m)),
            ViewportCommand::Maximized(true) => host::window_command(WindowCommand::Zoom),
            ViewportCommand::Fullscreen(on) => host::window_command(WindowCommand::Fullscreen(*on)),
            ViewportCommand::Focus => host::window_command(WindowCommand::Activate),
            ViewportCommand::Screenshot(data) => self.screenshots.push(data.clone()),
            ViewportCommand::RequestCut => self.events.push(Event::Cut),
            ViewportCommand::RequestCopy => self.events.push(Event::Copy),
            ViewportCommand::RequestPaste => self.paste(),
            _ => {}
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

/// A presentation by its name, as the editor tells one from other files it opens.
fn is_presentation(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [".deckcraft", ".slidecraft", ".pptx", ".potx", ".ppsx"].iter().any(|e| lower.ends_with(e))
}

/// egui's image copy, as PNG and as an archived BBitmap, which Haiku programs (ShowImage,
/// WonderBrush) paste.
fn copy_image(img: &egui::ColorImage) {
    let [w, h] = img.size;
    let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    let mut png = Vec::new();
    let encoded = image::RgbaImage::from_raw(w as u32, h as u32, rgba.clone())
        .ok_or_else(|| "bad size".to_string())
        .and_then(|i| i.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).map_err(|e| e.to_string()));
    match encoded {
        Ok(()) => {
            if !host::clipboard_set_image(None, &png, &rgba, w, h) {
                log::warn!("clipboard: couldn't copy the picture");
            }
        }
        Err(e) => log::warn!("clipboard picture: {e}"),
    }
}

/// Puts a PNG picture on the clipboard with `text`, also as an archived BBitmap. Whether it did.
fn copy_picture(text: Option<&str>, png: Vec<u8>) -> bool {
    let rgba = match image::load_from_memory_with_format(&png, image::ImageFormat::Png) {
        Ok(img) => img.to_rgba8(),
        Err(e) => {
            log::warn!("clipboard picture: {e}");
            return false;
        }
    };
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    let copied = host::clipboard_set_image(text, &png, rgba.as_raw(), w, h);
    if !copied {
        log::warn!("clipboard: couldn't copy a {w}x{h} picture");
    }
    copied
}
