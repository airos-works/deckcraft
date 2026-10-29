//! The air/OS side of [`crate::viewports`]: native windows are `host.cpp`'s `BDirectWindow`s,
//! frames are drawn with Vulkan (`gpu`, one presenter per window) or, without a hardware
//! adapter, by the software rasteriser (one canvas per window, the textures shared), and the
//! clipboard, cursors and links are the Be API's.
//!
//! Monitors: a slide show is the root window full screen, and the presenter view a window of its
//! own (upstream's `deckcraft_presenter` viewport, opened without a position). With several
//! monitors the two go on different ones: a window opened without a position goes on the active
//! window's monitor (the editor's, where the show was started), and a window going full screen
//! while another of ours is on its monitor fills a monitor none of ours is on instead. A window
//! opened without a position while the root is full screen goes on another monitor. With one
//! monitor the full-screen window floats above ordinary windows, so the other windows on it are
//! kept in front of it (floating too, until it leaves full screen), as the newer window is on
//! the other desktops.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use crate::host;
use crate::raster::{Canvas, Rasterizer};
use crate::services::{self, OBJECTS_MARKER};
use crate::viewports::{NativeCommand, Platform, WindowId, WindowInfo, WindowSpec};

/// What is drawn for one window.
#[derive(Default)]
struct Surface {
    canvas: Canvas,
    gpu: Option<crate::gpu::Surface>,
}

pub struct Haiku {
    /// The first window (the root viewport's), and the ones opened since, in order.
    root: WindowId,
    windows: Vec<WindowId>,
    /// Windows kept in front of a full-screen one on the same monitor (floating until it leaves).
    raised: Vec<WindowId>,
    /// Set when the editor copied shapes without text (its marker text): the shell puts a
    /// picture of them on the clipboard after the pass, which needs the session.
    pub copied_shapes: Rc<Cell<bool>>,
    raster: Rasterizer,
    /// The GPU renderer; `None` draws with `raster` (no hardware Vulkan adapter,
    /// `DECKCRAFT_RENDERER=cpu`, or the device was lost). The rasteriser keeps egui's textures
    /// either way, so it can take over between two frames.
    gpu: Option<crate::gpu::Gpu>,
    surfaces: HashMap<WindowId, Surface>,
}

/// Runs `f`; a panic inside it (a wgpu validation failure, a driver gone wrong) becomes an
/// error, so the windows fall back to the rasteriser instead of losing the open documents.
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

impl Haiku {
    pub fn new(root: WindowId) -> Result<Self, String> {
        Ok(Self {
            root,
            windows: vec![root],
            raised: Vec::new(),
            copied_shapes: Rc::default(),
            raster: Rasterizer::new(),
            gpu: start_gpu()?,
            surfaces: HashMap::new(),
        })
    }

    /// Full screen on, for `window`, on a monitor of its own when there are several (see the
    /// module documentation), or off.
    fn fullscreen(&mut self, window: WindowId, on: bool) {
        if !on {
            host::window_command(window, &NativeCommand::Fullscreen(false));
            for w in std::mem::take(&mut self.raised) {
                host::window_command(w, &NativeCommand::Floating(false));
            }
            return;
        }
        let monitors = host::monitors();
        let own = host::window_info(window).monitor;
        let others: Vec<(WindowId, WindowInfo)> = self
            .windows
            .iter()
            .filter(|w| **w != window)
            .map(|w| (*w, host::window_info(*w)))
            .filter(|(_, i)| !i.hidden && !i.minimized && i.inner[2] > i.inner[0])
            .collect();
        let busy = |m: &[f32; 4]| others.iter().any(|(_, i)| same_monitor(&i.monitor, m));
        let target = if monitors.len() > 1 && busy(&own) {
            monitors.iter().find(|m| !busy(m)).or_else(|| monitors.iter().find(|m| !same_monitor(m, &own))).copied()
        } else {
            None
        };
        log::debug!("window {window} full screen: monitors {monitors:?}, own {own:?}, on {target:?}");
        match target {
            Some(m) => host::fullscreen_on(window, [(m[0] + m[2]) / 2.0, (m[1] + m[3]) / 2.0]),
            None => host::window_command(window, &NativeCommand::Fullscreen(true)),
        }
        if monitors.len() > 1 {
            return;
        }
        // One monitor: the windows on it stay in front of the full-screen one.
        for (w, i) in others {
            if same_monitor(&i.monitor, &own) {
                host::window_command(w, &NativeCommand::Floating(true));
                host::window_command(w, &NativeCommand::Activate);
                self.raised.push(w);
            }
        }
    }

    /// Where a window opened without a position goes while the root is full screen: centred on
    /// another monitor, if there is one; else it floats in front of the full-screen window
    /// (then also true).
    fn place(&self, spec: &WindowSpec) -> (WindowSpec, bool) {
        let mut spec = spec.clone();
        let root = host::window_info(self.root);
        if spec.position.is_some() || !root.fullscreen || spec.floating {
            return (spec, false);
        }
        let monitors = host::monitors();
        match monitors.iter().find(|m| !same_monitor(m, &root.monitor)) {
            Some(m) => {
                let [w, h] = spec.size;
                spec.position = Some([(m[0] + (m[2] - m[0] - w) / 2.0).max(m[0]), (m[1] + (m[3] - m[1] - h) / 2.0).max(m[1])]);
                (spec, false)
            }
            None => {
                spec.floating = true;
                (spec, true)
            }
        }
    }

    /// Back to the software rasteriser for good: the device failed (`why`). The next frames
    /// draw everything with it.
    fn drop_gpu(&mut self, why: &str) {
        for surface in self.surfaces.values_mut() {
            // The presenters let go of the windows before the device goes.
            surface.gpu = None;
            surface.canvas.invalidate();
        }
        if self.gpu.take().is_some() {
            log::error!("GPU: {why}; switching to the software rasteriser");
        }
    }

    fn surface(&mut self, window: WindowId) -> &mut Surface {
        let gpu = &self.gpu;
        let surface = self.surfaces.entry(window).or_default();
        if surface.gpu.is_none()
            && let Some(gpu) = gpu
        {
            surface.gpu = Some(gpu.surface(window));
        }
        surface
    }
}

impl Drop for Haiku {
    fn drop(&mut self) {
        // The presenters let go of their windows (and the frame buffer) before the device goes.
        self.surfaces.clear();
    }
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
pub fn copy_picture(text: Option<&str>, png: Vec<u8>) -> bool {
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

/// Whether two monitor frames are the same monitor.
fn same_monitor(a: &[f32; 4], b: &[f32; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1.0)
}

impl Platform for Haiku {
    fn waker(&self) -> fn() {
        host::wake
    }

    fn open(&mut self, spec: &WindowSpec) -> Option<WindowId> {
        let (spec, raised) = self.place(spec);
        let window = host::open_window(&spec);
        log::debug!("window {window:?} opened: {spec:?}");
        if let Some(w) = window {
            self.windows.push(w);
            if raised {
                self.raised.push(w);
            }
        }
        window
    }

    fn close(&mut self, window: WindowId) {
        self.windows.retain(|w| *w != window);
        self.raised.retain(|w| *w != window);
        // The presenter stops listening to the window's direct connection first.
        self.surfaces.remove(&window);
        host::close_window(window);
        log::debug!("window {window} closed");
    }

    fn command(&mut self, window: WindowId, command: NativeCommand) {
        match command {
            NativeCommand::Fullscreen(on) => self.fullscreen(window, on),
            command => host::window_command(window, &command),
        }
    }

    fn info(&self, window: WindowId) -> WindowInfo {
        host::window_info(window)
    }

    fn set_cursor(&mut self, window: WindowId, cursor: egui::CursorIcon) {
        host::set_cursor(window, cursor_id(cursor));
    }

    fn output(&mut self, command: &egui::OutputCommand) {
        log::debug!("output {command:?}");
        match command {
            // Copied shapes without text (pictures, drawings) leave the editor's marker as their
            // text; the shell copies it with a picture of them.
            egui::OutputCommand::CopyText(t) if t == OBJECTS_MARKER => self.copied_shapes.set(true),
            egui::OutputCommand::CopyText(t) => {
                if !host::clipboard_set(t, None) {
                    log::warn!("clipboard: couldn't copy")
                }
            }
            egui::OutputCommand::CopyImage(img) => copy_image(img),
            // Haiku has one clipboard, no X11-style selection to keep the selected text in.
            egui::OutputCommand::TextSelectionSettled(_) => {}
            egui::OutputCommand::OpenUrl(u) => {
                if !host::open_url(&u.url) {
                    log::warn!("couldn't open {}", u.url);
                }
            }
        }
    }

    /// The clipboard's text, or, without text, an empty paste, which the editor answers with the
    /// clipboard's picture (`Services::clipboard_image`).
    fn clipboard_text(&self) -> Option<String> {
        services::clipboard_text().or_else(|| services::clipboard_has_picture().then(String::new))
    }

    fn dark(&self) -> bool {
        host::dark()
    }

    fn max_texture_side(&self) -> usize {
        self.gpu.as_ref().map_or(8192, |g| g.max_texture_side().min(16384))
    }

    fn set_textures(&mut self, delta: &egui::TexturesDelta) {
        // The rasteriser keeps them too, to take over from a failed GPU.
        self.raster.set_textures(delta);
        if let Some(gpu) = &mut self.gpu
            && let Err(e) = guard("GPU textures", || {
                gpu.set_textures(delta);
                Ok(())
            })
        {
            self.drop_gpu(&e);
        }
    }

    fn free_textures(&mut self, ids: &[egui::TextureId]) {
        self.raster.free_textures(ids);
        if let Some(gpu) = &mut self.gpu {
            gpu.free_textures(ids);
        }
    }

    fn paint(&mut self, window: WindowId, size: [u32; 2], primitives: &[egui::ClippedPrimitive], ppp: f32, clear: egui::Color32, scale: f32) {
        if self.gpu.as_ref().is_some_and(|g| g.faulted()) {
            self.drop_gpu("the device was lost or failed");
        }
        let t0 = Instant::now();
        let mut failed = None;
        if self.gpu.is_some() {
            // The presenter shows GPU frames itself.
            self.surface(window);
            if let (Some(gpu), Some(Surface { gpu: Some(surface), .. })) = (&mut self.gpu, self.surfaces.get_mut(&window))
                && let Err(e) = guard("GPU frame", || gpu.render(surface, size, primitives, ppp, clear, scale))
            {
                failed = Some(e);
            }
        }
        if let Some(e) = failed {
            self.drop_gpu(&e);
            // This frame too.
        } else if self.gpu.is_some() {
            log::debug!("window {window} frame {}x{}: drawn in {:.1} ms", size[0], size[1], t0.elapsed().as_secs_f64() * 1e3);
            return;
        }
        let (w, h) = (size[0] as usize, size[1] as usize);
        let surface = self.surfaces.entry(window).or_default();
        let dirty = self.raster.render(&mut surface.canvas, [w, h], primitives, ppp, clear);
        let t1 = Instant::now();
        if let Some(dirty) = dirty {
            host::present(window, surface.canvas.pixels(), w, h, dirty, scale);
        }
        log::debug!(
            "window {window} frame {w}x{h}: raster {:.1} ms, present {:.1} ms, dirty {dirty:?}",
            (t1 - t0).as_secs_f64() * 1e3,
            t1.elapsed().as_secs_f64() * 1e3
        );
    }

    fn screenshot(&mut self, window: WindowId) -> egui::ColorImage {
        let Some(surface) = self.surfaces.get_mut(&window) else { return egui::ColorImage::default() };
        match &mut surface.gpu {
            Some(gpu) => gpu.image().unwrap_or_else(|e| {
                log::warn!("screenshot: {e}");
                egui::ColorImage::default()
            }),
            None => surface.canvas.image(),
        }
    }

    fn ime_caret(&mut self, window: WindowId, caret: Option<[f32; 4]>) {
        host::ime_caret(window, caret);
    }

    fn ime_stop(&mut self, window: WindowId) {
        host::ime_stop(window);
    }

    fn chores_due(&self) -> Option<Instant> {
        self.surfaces
            .values()
            .filter_map(|s| s.gpu.as_ref())
            .filter_map(|g| if g.wants_present() { Some(Instant::now()) } else { g.sync_due() })
            .min()
    }

    /// The last frame again where a window moved on the screen, and app_server's copy catching
    /// up with frames that went straight to the screen.
    fn chores(&mut self, scale: f32) {
        let now = Instant::now();
        let mut failed = None;
        for surface in self.surfaces.values_mut() {
            let Some(gpu) = &mut surface.gpu else { continue };
            let result = if gpu.wants_present() {
                guard("GPU present", || gpu.present_again(scale))
            } else if gpu.sync_due().is_some_and(|at| at <= now) {
                guard("GPU sync", || gpu.sync(scale))
            } else {
                continue;
            };
            if let Err(e) = result {
                failed = Some(e);
                break;
            }
        }
        if let Some(e) = failed {
            self.drop_gpu(&e);
        }
    }
}
