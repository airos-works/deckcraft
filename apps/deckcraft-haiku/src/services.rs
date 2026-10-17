//! The app's system services on air/OS: Open and Save panels (`BFilePanel`), the clipboard
//! (`BClipboard`: text and HTML), links, and making DeckCraft the preferred application for
//! presentations (`BMimeType`).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use deckcraft_ui_egui::Services;

use crate::host::{self, RawEvent, kind};

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

/// Native panels, clipboard and Media Kit output for the unchanged presentation editor.
pub fn services(dialogs: Rc<Dialogs>) -> Services {
    let open = dialogs.clone();
    Services {
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
        clipboard_image: Some(Box::new(|| host::clipboard_get("image/png"))),
        clipboard_html: Some(Box::new(|| clip_text("text/html"))),
        clipboard_set_html: Some(Box::new(|html, text| host::clipboard_set(text, Some(html)))),
        audio_out: std::env::var_os("DECKCRAFT_NO_AUDIO")
            .is_none()
            .then(|| Box::new(host::AudioOut::default()) as Box<dyn deckcraft_media::AudioOut>),
        ..Default::default()
    }
}
