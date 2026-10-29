//! The native host (`native/host.cpp`): safe wrappers over its C functions. The only module with
//! `unsafe` code; every call here is a plain C call with owned or borrowed buffers.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_void};

/// An input or window event from the native host. Keep in step with `DcEvent` in `host.cpp`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RawEvent {
    pub kind: i32,
    pub buttons: i32,
    pub clicks: i32,
    pub modifiers: u32,
    pub key: i32,
    pub raw_char: i32,
    pub repeat: i32,
    pub id: i32,
    pub x: f32,
    pub y: f32,
    pub dx: f32,
    pub dy: f32,
    pub text_len: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RawWindowInfo {
    inner: [f32; 4],
    outer: [f32; 4],
    screen: [f32; 2],
    minimized: i32,
    active: i32,
    zoomed: i32,
    fullscreen: i32,
}

/// `be:wheel_phase` of an air/OS touchpad's scroll gesture (`input_wheel_phase` in `<Input.h>`),
/// a [`kind::WHEEL`] event's `clicks`.
pub mod wheel_phase {
    pub const BEGAN: i32 = 1;
    pub const ENDED: i32 = 3;
}

/// The kinds of [`RawEvent`].
pub mod kind {
    pub const MOUSE_MOVED: i32 = 1;
    pub const MOUSE_DOWN: i32 = 2;
    pub const MOUSE_UP: i32 = 3;
    pub const WHEEL: i32 = 4;
    pub const KEY_DOWN: i32 = 5;
    pub const KEY_UP: i32 = 6;
    pub const RESIZED: i32 = 7;
    pub const FOCUS: i32 = 8;
    pub const CLOSE: i32 = 9;
    pub const OPEN_FILES: i32 = 10;
    pub const DROPPED_FILES: i32 = 11;
    pub const POINTER_GONE: i32 = 12;
    pub const QUIT_REQUESTED: i32 = 13;
    pub const DIALOG_ANSWER: i32 = 14;
    pub const MODIFIERS: i32 = 15;
    pub const MOVED: i32 = 16;
    pub const MINIMIZED: i32 = 17;
    pub const COLORS: i32 = 18;
}

unsafe extern "C" {
    fn dc_host_init(signature: *const c_char) -> i32;
    fn dc_host_open_window(title: *const c_char, placed: i32, x: f32, y: f32, w: f32, h: f32, min_w: f32, min_h: f32, zoomed: i32) -> i32;
    fn dc_host_run();
    fn dc_host_quit();
    fn dc_host_wait(timeout_us: i64) -> i32;
    fn dc_host_wake();
    fn dc_host_next_event(event: *mut RawEvent) -> i32;
    fn dc_host_event_text() -> *const c_char;
    fn dc_host_present(pixels: *const u32, w: i32, h: i32, x0: i32, y0: i32, x1: i32, y1: i32, scale: f32);
    fn dc_host_present_rects(pixels: *const u32, w: i32, h: i32, rects: *const i32, count: i32, scale: f32);
    fn dc_host_direct_sink(sink: Option<unsafe extern "C" fn(*const RawDirect)>);
    fn dc_host_view_size(w: *mut f32, h: *mut f32);
    fn dc_host_window_info(info: *mut RawWindowInfo);
    fn dc_host_set_title(title: *const c_char);
    fn dc_host_window_command(command: i32, a: f32, b: f32);
    fn dc_host_set_cursor(id: i32);
    fn dc_host_modifiers() -> u32;
    fn dc_host_ui_scale() -> f32;
    fn dc_host_device_scale() -> f32;
    fn dc_host_dark() -> i32;
    fn dc_host_locale(languages: *mut *mut c_char) -> *mut c_char;
    fn dc_free(data: *mut c_void);
    fn dc_clipboard_get(mime: *const c_char, len: *mut usize) -> *mut c_void;
    fn dc_clipboard_set(text: *const c_char, text_len: usize, html: *const c_char, html_len: usize) -> i32;
    fn dc_clipboard_bitmap(w: *mut i32, h: *mut i32) -> *mut c_void;
    fn dc_clipboard_has_picture(types: *const c_char) -> i32;
    fn dc_clipboard_set_image(text: *const c_char, text_len: usize, png: *const c_void, png_len: usize, rgba: *const u8, w: i32, h: i32) -> i32;
    fn dc_host_file_panel(id: i32, save: i32, name: *const c_char, extensions: *const c_char, title: *const c_char) -> i32;
    fn dc_host_open_url(url: *const c_char) -> i32;
    fn dc_host_alert(title: *const c_char, text: *const c_char);
    fn dc_host_type_file(path: *const c_char) -> i32;
    fn dc_host_claim_types(signature: *const c_char, types: *const c_char) -> i32;
}

/// Text for C: interior NULs (never in what we pass) are dropped rather than failing.
fn c(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Takes a `malloc`'d C string from the host.
fn take_string(p: *mut c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the host returns NUL-terminated strings from strdup.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    // SAFETY: allocated by the host's malloc.
    unsafe { dc_free(p.cast()) };
    s
}

/// Takes `malloc`'d bytes from the host.
fn take_bytes(p: *mut c_void, len: usize) -> Option<Vec<u8>> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the host allocated `len` bytes at `p`.
    let v = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) }.to_vec();
    // SAFETY: allocated by the host's malloc.
    unsafe { dc_free(p) };
    Some(v)
}

/// Creates the BApplication. Call once, on the main thread, before anything else here.
pub fn init(signature: &str) -> Result<(), String> {
    let sig = c(signature);
    // SAFETY: a valid C string.
    match unsafe { dc_host_init(sig.as_ptr()) } {
        0 => Ok(()),
        e => Err(format!("the application couldn't start ({e})")),
    }
}

/// Where and how big the window opens: the frame's top-left corner, tab and border included
/// (`None`: centred; a position is fitted onto the monitors there are), and the content size.
pub struct WindowSpec {
    pub title: String,
    pub position: Option<[f32; 2]>,
    pub size: [f32; 2],
    pub min_size: [f32; 2],
    pub zoomed: bool,
}

pub fn open_window(spec: &WindowSpec) {
    let title = c(&spec.title);
    let [x, y] = spec.position.unwrap_or_default();
    let placed = i32::from(spec.position.is_some());
    // SAFETY: a valid C string; the host copies it.
    unsafe {
        dc_host_open_window(title.as_ptr(), placed, x, y, spec.size[0], spec.size[1], spec.min_size[0], spec.min_size[1], i32::from(spec.zoomed));
    }
}

/// Runs the application's message loop on this (the main) thread until [`quit`].
pub fn run() {
    // SAFETY: called once, after `init`.
    unsafe { dc_host_run() }
}

/// Ends the message loop (the window closes and [`run`] returns).
pub fn quit() {
    // SAFETY: no arguments.
    unsafe { dc_host_quit() }
}

/// Waits for an event, a [`wake`] or the timeout (`None`: no timeout).
pub fn wait(timeout: Option<std::time::Duration>) {
    let us = timeout.map_or(-1, |t| i64::try_from(t.as_micros()).unwrap_or(i64::MAX));
    // SAFETY: no pointers.
    unsafe { dc_host_wait(us) };
}

/// Ends a [`wait`] early; callable from any thread.
pub fn wake() {
    // SAFETY: no arguments.
    unsafe { dc_host_wake() }
}

/// The next queued event and its text.
pub fn next_event() -> Option<(RawEvent, String)> {
    let mut ev = RawEvent::default();
    // SAFETY: `ev` is a valid, writable `DcEvent`.
    if unsafe { dc_host_next_event(&mut ev) } == 0 {
        return None;
    }
    let text = if ev.text_len == 0 {
        String::new()
    } else {
        // SAFETY: the text stays valid until the next `dc_host_next_event`; it holds `text_len` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(dc_host_event_text().cast::<u8>(), ev.text_len as usize) };
        String::from_utf8_lossy(bytes).into_owned()
    };
    Some((ev, text))
}

/// Shows `pixels` (`width` × `height` device pixels, 0xAARRGGBB, `scale` per logical pixel) in
/// the window; only the `dirty` rectangle ([x0, y0, x1, y1), pixels) is copied and redrawn.
pub fn present(pixels: &[u32], width: usize, height: usize, dirty: [usize; 4], scale: f32) {
    if pixels.len() < width.saturating_mul(height) {
        return;
    }
    let to_i32 = |v: usize| i32::try_from(v).unwrap_or(i32::MAX);
    // SAFETY: `pixels` holds width × height values; the host only reads them.
    unsafe {
        dc_host_present(pixels.as_ptr(), to_i32(width), to_i32(height), to_i32(dirty[0]), to_i32(dirty[1]), to_i32(dirty[2]), to_i32(dirty[3]), scale)
    }
}

/// The window's direct connection as `host.cpp` reports it (`DcDirect`): where its content
/// is in the frame buffer, what of it is visible, or `connected` 0 when it may not draw there.
#[repr(C)]
struct RawDirect {
    connected: i32,
    /// Window bounds, frame buffer pixels, edges included.
    bounds: [i32; 4],
    bytes_per_row: u32,
    rect_count: u32,
    rects: *const i32,
}

/// Where the window's direct connection goes (the presenter's link), if anywhere.
static DIRECT_LINK: std::sync::Mutex<Option<std::sync::Arc<crate::present::DirectLink>>> = std::sync::Mutex::new(None);

/// The window's direct daemon thread, with `host.cpp`'s connection lock held.
unsafe extern "C" fn direct_connected(raw: *const RawDirect) {
    let link = DIRECT_LINK.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let Some(link) = link else { return };
    // SAFETY: host.cpp passes a valid report whose rectangles live for the call.
    let raw = unsafe { &*raw };
    if raw.connected == 0 || raw.rects.is_null() {
        link.connected(None);
        return;
    }
    // SAFETY: `rect_count` rectangles of four values each.
    let flat = unsafe { std::slice::from_raw_parts(raw.rects, raw.rect_count as usize * 4) };
    let rects: Vec<[i32; 4]> = flat.as_chunks::<4>().0.to_vec();
    let [left, top, right, bottom] = raw.bounds;
    let size = [u32::try_from(right - left + 1).unwrap_or(0), u32::try_from(bottom - top + 1).unwrap_or(0)];
    link.connected(Some(crate::present::Connection { origin: [left, top], size, rects: &rects, bytes_per_row: raw.bytes_per_row }));
}

/// The native window for the presenter (`present.rs`).
pub struct Native;

impl crate::present::Native for Native {
    fn show(&self, pixels: &[u32], size: [u32; 2], rects: &[[u32; 4]], scale: f32) {
        let [w, h] = size;
        if pixels.len() < (w as usize).saturating_mul(h as usize) || rects.is_empty() {
            return;
        }
        let flat: Vec<i32> = rects.iter().flatten().map(|&v| i32::try_from(v).unwrap_or(i32::MAX)).collect();
        let n = |v: usize| i32::try_from(v).unwrap_or(i32::MAX);
        // SAFETY: `pixels` holds w × h values and `flat` the rectangles; the host only reads them.
        unsafe { dc_host_present_rects(pixels.as_ptr(), n(w as usize), n(h as usize), flat.as_ptr(), n(rects.len()), scale) }
    }

    fn set_direct_link(&self, link: Option<std::sync::Arc<crate::present::DirectLink>>) {
        // host.cpp calls `direct_connected` with its lock held; take that lock first when
        // unsetting (it waits for a report in progress), and set ours first when setting (the
        // latest report is replayed at once).
        if link.is_some() {
            *DIRECT_LINK.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = link;
            // SAFETY: a function with the expected signature.
            unsafe { dc_host_direct_sink(Some(direct_connected)) }
        } else {
            // SAFETY: unsets it.
            unsafe { dc_host_direct_sink(None) }
            *DIRECT_LINK.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    fn waker(&self) -> fn() {
        wake
    }
}

/// The canvas size in logical pixels.
pub fn view_size() -> (f32, f32) {
    let (mut w, mut h) = (0.0, 0.0);
    // SAFETY: two writable floats.
    unsafe { dc_host_view_size(&mut w, &mut h) };
    (w, h)
}

/// The window's content and frame rectangles (logical screen pixels), the screen size and its
/// state.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowInfo {
    pub inner: [f32; 4],
    pub outer: [f32; 4],
    pub screen: [f32; 2],
    pub minimized: bool,
    pub active: bool,
    pub fullscreen: bool,
}

pub fn window_info() -> WindowInfo {
    let mut raw = RawWindowInfo::default();
    // SAFETY: a writable `DcWindowInfo`.
    unsafe { dc_host_window_info(&mut raw) };
    WindowInfo {
        inner: raw.inner,
        outer: raw.outer,
        screen: raw.screen,
        minimized: raw.minimized != 0,
        active: raw.active != 0,
        fullscreen: raw.fullscreen != 0,
    }
}

pub fn set_title(title: &str) {
    let t = c(title);
    // SAFETY: a valid C string; the host copies it.
    unsafe { dc_host_set_title(t.as_ptr()) }
}

/// What [`window_command`] does.
#[derive(Clone, Copy, Debug)]
pub enum WindowCommand {
    /// Frame's top-left corner to (x, y).
    MoveTo(f32, f32),
    /// Content size.
    ResizeTo(f32, f32),
    Minimize(bool),
    Zoom,
    Activate,
    MinSize(f32, f32),
    Fullscreen(bool),
}

pub fn window_command(cmd: WindowCommand) {
    let (id, a, b) = match cmd {
        WindowCommand::MoveTo(x, y) => (1, x, y),
        WindowCommand::ResizeTo(w, h) => (2, w, h),
        WindowCommand::Minimize(m) => (3, if m { 1.0 } else { 0.0 }, 0.0),
        WindowCommand::Zoom => (4, 1.0, 0.0),
        WindowCommand::Activate => (5, 0.0, 0.0),
        WindowCommand::MinSize(w, h) => (6, w, h),
        WindowCommand::Fullscreen(on) => (7, f32::from(u8::from(on)), 0.0),
    };
    // SAFETY: no pointers.
    unsafe { dc_host_window_command(id, a, b) }
}

/// One of Haiku's `BCursorID`s.
pub fn set_cursor(id: i32) {
    // SAFETY: no pointers; unknown ids fall back to the default cursor.
    unsafe { dc_host_set_cursor(id) }
}

/// Haiku's modifier key bits (`modifiers()`).
pub fn modifiers() -> u32 {
    // SAFETY: no arguments.
    unsafe { dc_host_modifiers() }
}

/// The interface scale: the system font size against 12 pt.
pub fn ui_scale() -> f32 {
    // SAFETY: no arguments.
    unsafe { dc_host_ui_scale() }
}

/// Frame buffer pixels per logical pixel: air/OS draws the desktop at its monitors' density
/// (2 at 200 %); 1 elsewhere. `DECKCRAFT_DEVICE_SCALE` overrides it.
pub fn device_scale() -> f32 {
    if let Some(s) = std::env::var("DECKCRAFT_DEVICE_SCALE").ok().and_then(|s| s.parse::<f32>().ok()).filter(|s| (0.5..=4.0).contains(s)) {
        return s;
    }
    // SAFETY: no arguments.
    let s = unsafe { dc_host_device_scale() };
    if s.is_finite() && (1.0..=4.0).contains(&s) { s } else { 1.0 }
}

/// Whether the system's panel colour is dark.
pub fn dark() -> bool {
    // SAFETY: no arguments.
    unsafe { dc_host_dark() != 0 }
}

/// The system's formatting conventions (`en_US`) and preferred languages (`en`, `de`…).
pub fn locale() -> (String, Vec<String>) {
    let mut langs: *mut c_char = std::ptr::null_mut();
    // SAFETY: a writable pointer; both strings come back from strdup.
    let id = take_string(unsafe { dc_host_locale(&mut langs) });
    let langs = take_string(langs).lines().map(str::to_string).filter(|l| !l.is_empty()).collect();
    (id, langs)
}

/// Sets the locale variables the interface reads (`LANG`, `LC_*`) from the system's settings,
/// where the environment doesn't have them (an app launched from Tracker or the Deskbar).
/// Call before any other thread starts.
pub fn export_locale() {
    if std::env::var_os("LANG").is_some() || std::env::var_os("LC_ALL").is_some() {
        return;
    }
    let (conventions, languages) = locale();
    let safe = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '@');
    let lang = languages.first().filter(|l| safe(l)).map(|l| l.replace('-', "_"));
    let lang = match (lang, safe(&conventions)) {
        // "de" with conventions "de_CH": de_CH; otherwise the language alone.
        (Some(l), true) if conventions.starts_with(&format!("{l}_")) => conventions.clone(),
        (Some(l), _) => l,
        (None, true) => conventions.clone(),
        (None, false) => return,
    };
    // SAFETY: called from `main` before any thread is spawned, so nothing reads the environment
    // concurrently.
    unsafe {
        std::env::set_var("LANG", format!("{lang}.UTF-8"));
        if safe(&conventions) {
            for var in ["LC_NUMERIC", "LC_MONETARY", "LC_PAPER", "LC_MEASUREMENT", "LC_TIME"] {
                if std::env::var_os(var).is_none() {
                    std::env::set_var(var, format!("{conventions}.UTF-8"));
                }
            }
        }
        if languages.len() > 1 && std::env::var_os("LANGUAGE").is_none() {
            std::env::set_var("LANGUAGE", languages.iter().filter(|l| safe(l)).map(|l| l.replace('-', "_")).collect::<Vec<_>>().join(":"));
        }
    }
}

/// The clipboard's data of a MIME type.
pub fn clipboard_get(mime: &str) -> Option<Vec<u8>> {
    let m = c(mime);
    let mut len = 0usize;
    // SAFETY: a valid C string and a writable length.
    let p = unsafe { dc_clipboard_get(m.as_ptr(), &mut len) };
    take_bytes(p, len)
}

/// Replaces the clipboard with `text` and, if given, its HTML.
pub fn clipboard_set(text: &str, html: Option<&str>) -> bool {
    let (h, hl) = html.map_or((std::ptr::null(), 0), |h| (h.as_ptr().cast::<c_char>(), h.len()));
    // SAFETY: the pointers and lengths describe live buffers the host copies.
    unsafe { dc_clipboard_set(text.as_ptr().cast(), text.len(), h, hl) == 0 }
}

/// A picture another Haiku program put on the clipboard as an archived BBitmap: width, height,
/// straight RGBA.
pub fn clipboard_bitmap() -> Option<(usize, usize, Vec<u8>)> {
    let (mut w, mut h) = (0i32, 0i32);
    // SAFETY: two writable ints; the pixels come back malloc'd, w × h × 4 bytes.
    let p = unsafe { dc_clipboard_bitmap(&mut w, &mut h) };
    let (w, h) = (usize::try_from(w).ok()?, usize::try_from(h).ok()?);
    let rgba = take_bytes(p, w.checked_mul(h)?.checked_mul(4)?)?;
    Some((w, h, rgba))
}

/// Whether the clipboard holds an archived BBitmap or data of one of the picture `types`.
pub fn clipboard_has_picture(types: &[&str]) -> bool {
    let t = c(&types.join("\n"));
    // SAFETY: a valid C string.
    unsafe { dc_clipboard_has_picture(t.as_ptr()) != 0 }
}

/// Replaces the clipboard with a picture: its PNG and, for Haiku programs, an archived BBitmap
/// of the straight-RGBA pixels (`width` × `height` × 4 bytes); with `text` when given.
pub fn clipboard_set_image(text: Option<&str>, png: &[u8], rgba: &[u8], width: usize, height: usize) -> bool {
    let (Ok(w), Ok(h)) = (i32::try_from(width), i32::try_from(height)) else { return false };
    if width.checked_mul(height).and_then(|n| n.checked_mul(4)) != Some(rgba.len()) {
        return false;
    }
    let (t, tl) = text.map_or((std::ptr::null(), 0), |t| (t.as_ptr().cast::<c_char>(), t.len()));
    // SAFETY: `rgba` holds width × height × 4 bytes (checked); the host copies every buffer.
    unsafe { dc_clipboard_set_image(t, tl, png.as_ptr().cast(), png.len(), rgba.as_ptr(), w, h) == 0 }
}

/// Shows an Open panel (offering `extensions`; `*` is any file) or a Save panel (suggesting
/// `name`); the answer comes back as a [`kind::DIALOG_ANSWER`] event with `id`.
pub fn file_panel(id: i32, save: bool, name: &str, extensions: &[&str], title: &str) {
    let (n, e, t) = (c(name), c(&extensions.join("\n")), c(title));
    // SAFETY: valid C strings; the host copies them.
    unsafe { dc_host_file_panel(id, i32::from(save), n.as_ptr(), e.as_ptr(), t.as_ptr()) };
}

/// Opens a URL with its preferred application.
pub fn open_url(url: &str) -> bool {
    let u = c(url);
    // SAFETY: a valid C string.
    unsafe { dc_host_open_url(u.as_ptr()) == 0 }
}

/// Sets a saved file's MIME type (`BEOS:TYPE`) from its extension and content, if it has none.
pub fn type_file(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(p) = CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: a valid C string.
    unsafe { dc_host_type_file(p.as_ptr()) == 0 }
}

/// Makes `signature` the preferred application of those of the MIME types that have none;
/// how many it claimed.
pub fn claim_types(signature: &str, types: &[&str]) -> i32 {
    let (s, t) = (c(signature), c(&types.join("\n")));
    // SAFETY: valid C strings.
    unsafe { dc_host_claim_types(s.as_ptr(), t.as_ptr()) }
}

/// A modal error alert.
pub fn alert(title: &str, text: &str) {
    let (t, x) = (c(title), c(text));
    // SAFETY: valid C strings; the alert copies them.
    unsafe { dc_host_alert(t.as_ptr(), x.as_ptr()) }
}

// The callback and all pointers stay in this FFI module. A mutex prevents overlapping device
// callbacks from aliasing the FnMut, and the native stop joins the callback before freeing it.
type AudioFill = Box<dyn FnMut(&mut [f32], usize, u32) + Send>;
struct AudioCookie {
    fill: std::sync::Mutex<AudioFill>,
    failed: std::sync::atomic::AtomicBool,
}
#[derive(Default)]
pub struct AudioOut {
    handle: Option<std::ptr::NonNull<c_void>>,
    cookie: Option<Box<AudioCookie>>,
}
unsafe extern "C" {
    fn dc_audio_start(cookie: *mut c_void, fill: unsafe extern "C" fn(*mut c_void, *mut f32, usize, usize, u32)) -> *mut c_void;
    fn dc_audio_stop(handle: *mut c_void);
}
unsafe extern "C" fn fill_audio(cookie: *mut c_void, samples: *mut f32, count: usize, channels: usize, rate: u32) {
    // SAFETY: BSoundPlayer supplies a writable aligned float buffer; our Box stays alive until
    // dc_audio_stop has joined the device thread. Neither reference escapes this call.
    let (cookie, samples) = unsafe { (&*cookie.cast::<AudioCookie>(), std::slice::from_raw_parts_mut(samples, count)) };
    let Ok(mut fill) = cookie.fill.try_lock() else { return };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fill(samples, channels, rate))).is_err() {
        samples.fill(0.0);
        cookie.failed.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
impl deckcraft_media::AudioOut for AudioOut {
    fn start(&mut self, fill: AudioFill) -> Result<(), String> {
        self.stop();
        let mut cookie = Box::new(AudioCookie { fill: std::sync::Mutex::new(fill), failed: false.into() });
        // SAFETY: cookie is heap allocated and remains at the same address until stop joins.
        let handle = unsafe { dc_audio_start((&mut *cookie as *mut AudioCookie).cast(), fill_audio) };
        let Some(handle) = std::ptr::NonNull::new(handle) else { return Err("Media Kit audio output is unavailable".into()) };
        self.cookie = Some(cookie);
        self.handle = Some(handle);
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: this is our live player, stopped exactly once, before dropping its cookie.
            unsafe { dc_audio_stop(handle.as_ptr()) };
        }
        self.cookie = None;
    }
    fn failure(&mut self) -> Option<String> {
        self.cookie.as_ref().filter(|c| c.failed.swap(false, std::sync::atomic::Ordering::Relaxed)).map(|_| "audio callback failed".into())
    }
}
impl Drop for AudioOut {
    fn drop(&mut self) {
        deckcraft_media::AudioOut::stop(self);
    }
}

// The Media Kit's codecs (`native/media.cpp`), for `media_kit`. The media's bytes are Rust's: a
// decoder keeps its own reference to them and closes its native handle before letting them go.

/// What the Media Kit says about media bytes. Keep in step with `DcMediaInfo` in `media.cpp`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MediaKitInfo {
    container: [c_char; 32],
    pub duration_us: i64,
    pub has_video: i32,
    pub video_decodable: i32,
    pub width: i32,
    pub height: i32,
    pub fps: f32,
    pub frames: i64,
    video_codec: [c_char; 96],
    pub has_audio: i32,
    pub audio_decodable: i32,
    pub channels: i32,
    pub rate: f32,
    audio_codec: [c_char; 96],
}

impl MediaKitInfo {
    fn text(s: &[c_char]) -> String {
        let bytes: Vec<u8> = s.iter().take_while(|&&b| b != 0).map(|&b| b as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
    /// The reader's name for the container ("avi", "asf", "mpeg", "ogg"…).
    pub fn container(&self) -> String {
        Self::text(&self.container)
    }
    pub fn video_codec(&self) -> String {
        Self::text(&self.video_codec)
    }
    pub fn audio_codec(&self) -> String {
        Self::text(&self.audio_codec)
    }
}

unsafe extern "C" {
    fn dc_media_probe(data: *const c_void, len: usize, info: *mut MediaKitInfo) -> i32;
    fn dc_video_open(data: *const c_void, len: usize, width: *mut i32, height: *mut i32, container: *mut c_char) -> *mut c_void;
    fn dc_video_next(video: *mut c_void, rgba: *mut u8, len: usize, time_us: *mut i64) -> i32;
    fn dc_video_seek(video: *mut c_void, time_us: i64) -> i64;
    fn dc_video_close(video: *mut c_void);
    fn dc_audio_decode(data: *const c_void, len: usize, max_samples: usize, channels: *mut i32, rate: *mut f32, count: *mut usize) -> *mut f32;
}

/// The Media Kit's description of `data`, when one of its readers takes it.
pub fn media_probe(data: &[u8]) -> Option<MediaKitInfo> {
    // SAFETY: all-zero is a valid MediaKitInfo (integers and empty strings).
    let mut info: MediaKitInfo = unsafe { std::mem::zeroed() };
    // SAFETY: `data` is live for the call; the host fills `info` and keeps no pointer.
    (unsafe { dc_media_probe(data.as_ptr().cast(), data.len(), &mut info) } == 0).then_some(info)
}

/// The first audio track of `data` decoded by the Media Kit: channels, sample rate and
/// interleaved samples (at most `max_samples`).
pub fn media_audio(data: &[u8], max_samples: usize) -> Option<(u16, u32, Vec<f32>)> {
    let (mut channels, mut rate, mut count) = (0i32, 0f32, 0usize);
    // SAFETY: `data` is live for the call; the samples come back malloc'd, `count` of them.
    let p = unsafe { dc_audio_decode(data.as_ptr().cast(), data.len(), max_samples, &mut channels, &mut rate, &mut count) };
    if p.is_null() {
        return None;
    }
    // SAFETY: the host allocated `count` floats at `p`.
    let samples = unsafe { std::slice::from_raw_parts(p, count) }.to_vec();
    // SAFETY: allocated by the host's malloc.
    unsafe { dc_free(p.cast()) };
    let rate = rate.round();
    ((1.0..=768_000.0).contains(&rate) && channels > 0).then(|| (u16::try_from(channels).unwrap_or(1), rate as u32, samples))
}

/// A video track the Media Kit decodes, frame by frame.
pub struct MediaKitVideo {
    handle: std::ptr::NonNull<c_void>,
    /// The bytes the native reader reads; dropped only after the handle is closed.
    _bytes: std::sync::Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    /// The reader's name for the container, as [`MediaKitInfo::container`].
    pub container: String,
}

// SAFETY: the native decoder has no thread affinity; `&mut self` keeps it to one thread at a time.
unsafe impl Send for MediaKitVideo {}

impl MediaKitVideo {
    pub fn open(bytes: std::sync::Arc<Vec<u8>>) -> Option<Self> {
        let (mut w, mut h, mut container) = (0i32, 0i32, [0 as c_char; 32]);
        // SAFETY: the bytes stay alive (in `_bytes`) until the handle is closed in `drop`; the
        // host writes at most 32 bytes of container name.
        let p = unsafe { dc_video_open(bytes.as_ptr().cast(), bytes.len(), &mut w, &mut h, container.as_mut_ptr()) };
        let handle = std::ptr::NonNull::new(p)?;
        let container = MediaKitInfo::text(&container);
        Some(Self { handle, _bytes: bytes, width: u32::try_from(w).unwrap_or(0), height: u32::try_from(h).unwrap_or(0), container })
    }

    /// Decodes the next frame into `rgba` (resized to width × height × 4): its time in
    /// microseconds, `Ok(None)` at the end.
    pub fn next(&mut self, rgba: &mut Vec<u8>) -> Result<Option<i64>, ()> {
        rgba.resize(self.width as usize * self.height as usize * 4, 0);
        let mut time = 0i64;
        // SAFETY: our live handle and a writable buffer of the length passed.
        match unsafe { dc_video_next(self.handle.as_ptr(), rgba.as_mut_ptr(), rgba.len(), &mut time) } {
            1 => Ok(Some(time)),
            0 => Ok(None),
            _ => Err(()),
        }
    }

    /// Moves to the key frame at or before `time_us`; the time decoding resumes at.
    pub fn seek(&mut self, time_us: i64) -> i64 {
        // SAFETY: our live handle.
        unsafe { dc_video_seek(self.handle.as_ptr(), time_us) }
    }
}

impl Drop for MediaKitVideo {
    fn drop(&mut self) {
        // SAFETY: our live handle, closed exactly once, before `_bytes` is dropped.
        unsafe { dc_video_close(self.handle.as_ptr()) };
    }
}
