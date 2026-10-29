//! The app's system services on air/OS: Open and Save panels (`BFilePanel`), the clipboard
//! (`BClipboard`: text, HTML and pictures, as PNG and archived BBitmaps), links, printing (the
//! print kit, `src/print.rs`), and making DeckCraft the preferred application for presentations
//! (`BMimeType`).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, channel};

use deckcraft_ui_egui::Services;

use crate::host::{self, RawEvent, kind};
use crate::print;

/// The application's signature (also in `DeckCraft.rdef`).
pub const SIGNATURE: &str = "application/x-vnd.storytold-DeckCraft";

/// The presentation types DeckCraft opens: it becomes their preferred application on start where
/// none is set ([`claim_document_types`]), so a double-click in Tracker opens them.
const DOCUMENT_TYPES: &[&str] = &[
    "application/x-deckcraft",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/vnd.openxmlformats-officedocument.presentationml.template",
    "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
];

pub fn claim_document_types() {
    host::claim_types(SIGNATURE, DOCUMENT_TYPES);
}

/// The file panels. The app asks for a path and waits for it (`Services::pick_open` returns
/// one), so the interface thread waits for the panel's answer; whatever else the window sends
/// meanwhile is kept for the shell, in order.
#[derive(Default)]
pub struct Dialogs {
    next: Cell<i32>,
    /// Events that arrived while a panel was open.
    pub deferred: RefCell<VecDeque<(RawEvent, String)>>,
    /// Paths picked in Save panels, for the shell to type once written.
    pub saved: RefCell<Vec<PathBuf>>,
}

impl Dialogs {
    fn pick(&self, save: bool, name: &str, extensions: &[&str], title: &str) -> Option<String> {
        let id = self.next.get().wrapping_add(1);
        self.next.set(id);
        host::file_panel(id, save, name, extensions, &format!("DeckCraft: {title}"));
        loop {
            host::wait(None);
            while let Some((ev, text)) = host::next_event() {
                if ev.kind == kind::DIALOG_ANSWER && ev.id == id {
                    let picked = ev.x != 0.0 && !text.is_empty();
                    if picked && save {
                        self.saved.borrow_mut().push(PathBuf::from(&text));
                    }
                    return picked.then_some(text);
                }
                self.deferred.borrow_mut().push_back((ev, text));
            }
        }
    }
}

fn clip_text(mime: &str) -> Option<String> {
    let bytes = host::clipboard_get(mime)?;
    let s = String::from_utf8_lossy(&bytes).trim_end_matches('\0').to_string();
    (!s.is_empty()).then_some(s)
}

/// The clipboard's plain text.
pub fn clipboard_text() -> Option<String> {
    clip_text("text/plain")
}

/// Picture types other programs put on the clipboard encoded, best first.
const PICTURE_TYPES: [&str; 6] = ["image/png", "image/jpeg", "image/gif", "image/webp", "image/bmp", "image/tiff"];

/// Larger encoded pictures on the clipboard are not read.
const MAX_PICTURE_BYTES: usize = 256 << 20;

/// Whether there's a picture to paste (the shell's Paste with no text on the clipboard).
pub fn clipboard_has_picture() -> bool {
    host::clipboard_has_picture(&PICTURE_TYPES)
}

fn png_of(width: usize, height: usize, rgba: Vec<u8>) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(u32::try_from(width).ok()?, u32::try_from(height).ok()?, rgba)?;
    let mut png = Vec::new();
    match img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png) {
        Ok(()) => Some(png),
        Err(e) => {
            log::warn!("clipboard picture: {e}");
            None
        }
    }
}

/// The text DeckCraft copies with shapes (`OBJECTS_MARKER` in deckcraft-ui-egui), beside their
/// picture for other programs (`shell::copy_picture`).
pub const OBJECTS_MARKER: &str = "\u{2060}";

/// The clipboard's picture as PNG (what the editor's `clipboard_image` takes): a BBitmap
/// another Haiku program copied (ShowImage, Screenshot, WonderBrush, airShot), else PNG, else
/// another encoded picture, converted. Not the picture of shapes DeckCraft copied: pasting
/// those pastes the shapes themselves.
fn clipboard_image() -> Option<Vec<u8>> {
    if clipboard_text().as_deref() == Some(OBJECTS_MARKER) {
        return None;
    }
    if let Some((w, h, rgba)) = host::clipboard_bitmap() {
        return png_of(w, h, rgba);
    }
    if let Some(png) = host::clipboard_get("image/png") {
        return Some(png);
    }
    let bytes = PICTURE_TYPES[1..].iter().find_map(|m| host::clipboard_get(m)).filter(|b| b.len() <= MAX_PICTURE_BYTES)?;
    match image::load_from_memory(&bytes) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width() as usize, rgba.height() as usize);
            png_of(w, h, rgba.into_raw())
        }
        Err(e) => {
            log::warn!("clipboard picture: {e}");
            None
        }
    }
}

/// Native panels, clipboard and Media Kit output for the unchanged presentation editor.
///
/// Of the asynchronous hooks only `inbox` is set: the shell reads files dropped on the window on
/// a thread and delivers them there (`Shell::open_files`). `open_async` and `download` are the
/// web's stand-ins for a file panel; the editor asks them only when `pick_open` / `pick_save`
/// are missing, and it opens a presentation picked in the Open panel in the background itself
/// (`open_path_in_background`), so the panels stay.
/// File › Print's Print: the PDF of the Print page's pages (slides, notes pages or handouts, on
/// its paper) drawn into a `BPrintJob` on a worker. DeckCraft's Print page has no printer list,
/// so the system's print setup asks for the printer (its window has the printer menu) and then
/// shows that printer's page and job setup (paper, copies, the driver's options); their answers
/// are kept for each printer in `printers` (`~/config/settings/DeckCraft/printers`).
fn submit_print(title: &str, pdf: Vec<u8>, printers: Option<PathBuf>) -> Receiver<Result<String, String>> {
    let (tx, rx) = channel();
    let options = print::Options { title: title.to_owned(), setup: true, ..print::Options::default() };
    let store = print::Store::new(printers);
    let spawned = std::thread::Builder::new().name("print".into()).spawn(move || {
        let started = std::time::Instant::now();
        let result = print::print_pdf(None, pdf, &options, &store);
        match &result {
            Ok(receipt) => log::info!("printed in {:.1} s: {receipt}", started.elapsed().as_secs_f32()),
            Err(e) => log::warn!("printing: {e}"),
        }
        let _ = tx.send(result.map_err(|e| if e == print::Error::Cancelled { String::new() } else { e.to_string() }));
    });
    if let Err(e) = spawned {
        log::error!("starting the print worker: {e}");
    }
    rx
}

/// The app's services; `printers` keeps each printer's setup answers (none: not kept).
pub fn services(dialogs: Rc<Dialogs>, printers: Option<PathBuf>) -> Services {
    let open = dialogs.clone();
    Services {
        inbox: Some(deckcraft_ui_egui::Inbox::default()),
        pick_open: Some(Box::new(move |purpose: &str| {
            let (extensions, title): (&[&str], &str) = match purpose {
                "picture" => (&["png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff"], "Insert Picture"),
                "audio" => (
                    &["wav", "mp3", "m4a", "m4b", "aac", "flac", "ogg", "oga", "opus", "aif", "aiff", "aifc", "caf", "wma", "weba", "mka"],
                    "Insert Audio",
                ),
                "video" => (&["mp4", "m4v", "mov", "webm", "mkv", "wmv"], "Insert Video"),
                "theme" => (&["thmx", "pptx", "potx", "ppsx"], "Open Theme"),
                _ => (&["deckcraft", "pptx", "potx", "ppsx", "txt", "md"], "Open Presentation"),
            };
            open.pick(false, "", extensions, title)
        })),
        pick_save: Some(Box::new(move |name: &str| dialogs.pick(true, name, &[], "Save As"))),
        read: Some(Box::new(|path| std::fs::read(path).map_err(|e| e.to_string()))),
        write: Some(Box::new(|path, bytes| {
            std::fs::write(path, bytes).map_err(|e| e.to_string())?;
            host::type_file(std::path::Path::new(path));
            Ok(())
        })),
        clipboard_image: Some(Box::new(clipboard_image)),
        print: Some(Box::new(move |title, pdf| submit_print(title, pdf, printers.clone()))),
        clipboard_html: Some(Box::new(|| clip_text("text/html"))),
        clipboard_set_html: Some(Box::new(|html, text| host::clipboard_set(text, Some(html)))),
        // File › Info › Open file location: the presentation selected in its folder's Tracker
        // window (the file:// link of the folder it opens elsewhere is no link for BUrl when the
        // path has a space, and another folder when it has a '#').
        reveal: Some(Box::new(|path| {
            if host::reveal(std::path::Path::new(path)) { Ok(()) } else { Err(format!("Couldn't show {path} in Tracker")) }
        })),
        audio_out: std::env::var_os("DECKCRAFT_NO_AUDIO")
            .is_none()
            .then(|| Box::new(host::AudioOut::default()) as Box<dyn deckcraft_media::AudioOut>),
        ..Default::default()
    }
}
