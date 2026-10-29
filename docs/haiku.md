# DeckCraft on air/OS

Status: initial x86_64 port, validated on the air/OS laptop on 2026-10-18.

This fork runs the existing DeckCraft presentation editor and engine inside a native Be API
host. It reuses GridCraft's and WordCraft's air/OS window, input and software rendering code,
and WordCraft's recovery owner-lock fix. No sibling repository is needed to build it.

| Service | air/OS implementation |
| --- | --- |
| Window and event loop | `BApplication`, a `BWindow` and `BView` per egui viewport (editor, New Window, presenter view) |
| Drawing | Vulkan (egui-wgpu) on a hardware GPU, changed tiles shown in a `BBitmap`; else egui triangles rasterized on the CPU with dirty bands; HiDPI support |
| Open / Save / insert media | `BFilePanel` |
| Clipboard | `BClipboard`: text, HTML, and pictures as PNG and archived `BBitmap`s (ShowImage, Screenshot, WonderBrush, airShot) both ways |
| Cursors / locale / document types | `BCursor`, `BLocaleRoster`, `BMimeType` |
| Slide show | Native borderless full screen; with several monitors on one apart from the presenter view's window; restores the editor's frame |
| Printing | The print kit (`BPrintJob`): the Print page's PDF rasterised with hayro and spooled to a printer the system's print setup picks |
| Audio | `BSoundPlayer` (Media Kit), stereo float PCM; DeckCraft's existing decoder and mixer |
| Movies | DeckCraft's own decoders (H.264, HEVC, VP9, AV1); others through the Media Kit (`BMediaFile`, Movies) |
| Window | Opens where it was last closed (`window.json`), fitted onto the monitors there are now |
| Opening files | Tracker's and dropped presentations open in the background; dropped pictures, sounds and movies are inserted; one running copy (`B_SINGLE_LAUNCH`) |
| Recovery | Haiku `flock` through rustix; copies started from other files keep their snapshots |

The full ribbon, slide canvas, thumbnails, notes, editing commands, PPTX and `.deckcraft`
formats, PDF/image export, transitions and animations are the upstream Rust implementation.
The platform host replaces eframe/winit and cpal, and draws with wgpu itself (Rendering). The
only unsafe Rust is the reviewed native boundary in `apps/deckcraft-haiku/src/host.rs` and the
raw Vulkan frame buffer import in `src/scanout.rs`; the rest denies unsafe code. C++ is
limited to the Be API boundary (`native/host.cpp`, and `native/media.cpp` for the Media Kit's
codecs), following the existing sibling ports. The shared crates' only air/OS hook is
`deckcraft_media::platform`, safe Rust that any host could use.

Settings and logs live under `~/config/settings/DeckCraft`, recovery under `Recovery/`.
Fonts are discovered in the system and user `data/fonts` folders. The interface prefers
installed Noto Sans; no fonts are bundled. Control and Command (left Alt by default) both
map to Ctrl-style editing shortcuts. Option maps to Alt. The native window's close button
honors the editor's Save Changes prompt.

## Rendering

The window is drawn with Vulkan when the machine has a hardware adapter, as PhotoCraft's and
LightCraft's air/OS hosts do (`apps/deckcraft-haiku/src/gpu.rs`): egui's triangles are drawn by
egui-wgpu into a BGRA texture, and the presenter (`src/present.rs`, `damage.rs`, `scanout.rs`,
shared with PhotoCraft) compares it with what app_server shows, 64 px tile by tile, and copies
back only the tiles that changed. A frame drawn from exactly the same triangles, size and
textures as the one on the screen is not drawn again (egui repaints more often than its output
changes), so an idle or hovering window costs almost nothing. The software rasteriser
(`src/raster.rs`, dirty bands) remains for machines without a hardware Vulkan adapter, and takes
over without losing the open documents if the device is lost or reports an error.

`DECKCRAFT_RENDERER=auto|vulkan|cpu` picks the renderer (auto: Vulkan on a discrete or integrated
GPU, else the rasteriser; vulkan: fail at start without one). `DECKCRAFT_PRESENT=readback|full|direct`
picks how frames reach the window (`readback`, the default; `full`, the whole frame every time;
`direct`, experimental, straight into the screen's frame buffer through air/OS's
`VkImportScanoutMemoryHAIKU`; not the default because RADV on the Framework 13 loses the device a
few seconds after the import). wgpu's Vulkan backend is enabled for Haiku by the vendored
`wgpu`, `wgpu-core` and `wgpu-core-deps-windows-linux-android` 30.0.1 (`vendor/README.md`).

## Build

Use the existing Linux cross toolchain and air/OS development sysroot:

```sh
bash tools/haiku/build.sh
bash tools/haiku/package.sh --no-build
```

Output: `target/x86_64-unknown-haiku/release/DeckCraft` and
`artifacts/haiku/deckcraft-0.12.0~airos1-1-x86_64.hpkg`.

`tools/haiku/env.sh` sets defaults for this workspace. Override `RUSTUP_HOME`, `CARGO_HOME`,
`HAIKU_SYSROOT`, `HAIKU_CROSS`, `CARGO_TARGET_DIR`, and `HAIKU_HOST_TOOLS` for other machines.
Rust must satisfy the workspace version and include `rust-src`. The tier-3 Haiku target uses
`-Zbuild-std=std,panic_abort` with `RUSTC_BOOTSTRAP=1`, as the sibling ports do.
The host tools directory must contain `rc/rc`, `xres`, `resattr/resattr`, and `package/package`.
The package can be built entirely on Linux; its resources include DeckCraft's original
Haiku-style presentation-screen HVIF icon, generated by `tools/haiku/make-icon.py`.
The resources also include `BEOS:TYPE = application/x-vnd.Be-elfexecutable`,
which must survive into the package's attributes. Without it, Haiku treats the
binary as a script and can lose Tracker's initial document-open message.

Install the package using `pkgman install <package path>`. DeckCraft appears in the Deskbar's
Applications menu. The post-install script registers it for PPTX/POTX/PPSX and `.deckcraft`
where no preferred application has already been chosen.

```sh
DeckCraft --sample
DeckCraft --sample --show
DeckCraft --control 7990 /boot/home/talk.pptx
```

The optional localhost control channel is unchanged; see [control-protocol.md](control-protocol.md).
`DECKCRAFT_DEVICE_SCALE` overrides display density; `DECKCRAFT_NO_AUDIO` disables audio;
`DECKCRAFT_NO_PREFS` and `DECKCRAFT_NO_RECOVERY` are useful for isolated validation runs.

## Validation

Run `cargo xtask ci` on Linux for the upstream checks and the portable host tests, and
`bash tools/haiku/build.sh` for the actual native ABI. The rasterizer and input mapping tests
also run independently with `cargo test -p deckcraft-haiku`.

Validated on the laptop with the installed package:

- Sample slides render at the desktop's 175% density, with installed Noto Sans.
- Editing, PPTX save/reopen, PDF and PNG exports, native document-open messages and file panel cancellation.
- Fullscreen starts on the window's monitor; next-slide navigation and Escape restore the editor's frame.
- Closing a dirty document prompts to save, including when closing during a show.
- WAV playback advances through Media Kit's audio callback without an error (silent PCM used for the automated test).
- Two live instances (from different files) do not recover each other's unsaved work; after the owner exits, its text is recovered and can be saved.
- Package installation registers the app, icon, Deskbar entry and `.deckcraft` extension.

Linux checks need ALSA development files. A font covering CJK (BIZ UDPGothic, OFL) is needed
for the upstream vertical-text rendering test. The upstream notes-spelling UI test can fail
under parallel test execution; it passes in isolation. Run the suite with `RUST_TEST_THREADS=1`
for deterministic verification on this build host. The native Clippy check needs
`-A clippy::missing_const_for_thread_local` with this Rust 1.99 Haiku std build, which otherwise
reports that lint on an upstream thread-local initializer that is already `const`.

The final Linux run passed all seven `cargo xtask ci` gates, with 1,696 tests passing,
0 failures and 6 ignored tests. The native cross-build, native Clippy check (with the
single toolchain exception above), asset/layer checks and WebAssembly checks also passed.

## Movies

Movies embedded in slides play in the editor and the slide show with the movie's control bar
(play/pause, seek, loop, trim, fades), as on the other platforms: upstream's `VideoFeed` decodes
ahead of the playback clock on a worker thread and the frame due at the clock's time becomes the
shape's egui texture. The clock is the audio callback's (`BSoundPlayer`), so picture and sound
stay together. H.264, HEVC, VP9 and AV1 in MP4/MOV/WebM/Matroska use DeckCraft's own decoders.

Everything else goes through the Media Kit (`apps/deckcraft-haiku/native/media.cpp`,
`src/media_kit.rs`), registered with `deckcraft_media::platform` when the app starts: probing,
audio decoding and video decoding ask it only for what DeckCraft's decoders can't read.
`BMediaFile` reads the media's bytes from memory (`BMemoryIO`), `BMediaTrack` decodes video to
`B_RGB32` frames (converted to RGBA) and audio to float samples. Checked on the x399 (hrev60207,
ffmpeg 6 plugin, libavcodec 60.31): Motion JPEG (MOV), MPEG-2 (`.mpg`), MPEG-4 Part 2 (AVI,
MP4), MS-MPEG-4 v3 and WMV 8 with WMA 2 (`.wmv`), Theora with Vorbis (`.ogv`), and VP8 (WebM),
plus MP3, MP2, PCM and WMA audio in them. The ffmpeg plugin offers every decoder its libavcodec
has (WMV 1-3/VC-1, ProRes, DNxHD, Cinepak, Indeo, RealVideo, Sorenson, FLV, DV…).
`DECKCRAFT_NO_MEDIA_KIT=1` turns it off.

Media Kit seeking lands on the key frame before the time; the frames up to the time are decoded
and dropped, as DeckCraft's decoder does. Its Ogg and MPEG program stream readers only estimate
where a time is (Theora lands on a frame that isn't a key frame and shows the old picture with
new changes on it; MPEG lands after the time), so a seek there decodes from the start instead.
The ffmpeg WebM reader's frame count is wrong (hundreds of millions); the probe uses the duration
times the frame rate instead.

## Printing

File › Print prints through the Be print kit (`apps/deckcraft-haiku/src/print.rs` and
`native/print.cpp`, WordCraft's, unchanged). The editor makes the same PDF it exports, with the
Print page's layout (full-page slides, notes pages or handouts with 1-9 slides a page), paper,
orientation, frame and slides, and hands it to the host through a new `Services::print` hook;
without the hook (the other desktops) Print opens that PDF in a viewer as before. Each page is
rasterised with hayro at the printer's resolution (at most 300 dpi), put on the printer's paper
at its real size (shrunk when it doesn't fit; a landscape page turned on portrait paper), and
only the strips with ink are spooled. DeckCraft's Print page has no printer list, so the
system's print setup asks: its window has the printer menu (it remembers the printer chosen for
DeckCraft), then that printer's paper and job setup (copies, pages, the driver's options); the
answers are kept per printer in `~/config/settings/DeckCraft/printers`. The job is sent on a
worker with the driver's progress window, which can cancel it; the status bar then says how many
sheets were spooled, or that printing was cancelled. Quitting waits for a job being sent.

## Windows, the slide show and the presenter view

Every egui viewport gets a native window of its own, as eframe gives them on the other desktops
(WordCraft's design, `apps/deckcraft-haiku/src/viewports.rs`, platform-neutral and tested on any
host with a fake platform in `viewports/tests.rs`; `src/platform.rs` is its air/OS side). The
root viewport is the editor's window, which also becomes the slide show; View › New Window
(`deckcraft_doc_window`) and the slide show's presenter view (`deckcraft_presenter`) are
immediate viewports in windows of their own. `host.cpp` keeps its windows in a map by id and
tags every event with its window; each window has its own Vulkan presenter (one device and egui
renderer) or rasteriser canvas (one texture store), its own direct connection, cursor, full
screen state and input method transaction. Input goes to the window it happened in: a key or a
click in the presenter view drives the show as upstream's presenter window does. A window's
close button closes that window (the presenter view's ends it for the rest of the show, as
upstream); the editor's quits after asking about unsaved presentations, as before. Files dropped
on any window go to the editor's inbox. The editor answers its own IME resets
(`IMEOutput::reset_request`) per window: the host stops the input method and reports
`Event::ImeResetCompleted` after the input queued before it.

The presenter view is on by default again, as upstream (Slide Show › Presenter View, saved in
`ui.json`). The monitors come from air/OS's display layout (`BPrivate::get_display_layout`:
the enabled, connected monitors that don't mirror another; `dc_host_monitors`), and a window's
monitor from `get_display_frame`:

- A window opened without a position (the presenter view, a New Window) is centred on the
  active window's monitor: the editor's, where the show was started.
- A window going full screen while another of DeckCraft's windows is on its monitor fills a
  monitor none of them is on. So the slide show goes to the other monitor and the presenter view
  stays by the editor, as PowerPoint puts them. Without the presenter view the show fills the
  editor's monitor, as before. Leaving the show puts the editor back where it was.
- With one monitor (what upstream's desktop app does: the presenter window is a window of its
  own, in front of the full-screen show) the show fills the monitor above other programs'
  windows (`B_FLOATING_ALL_WINDOW_FEEL`, as before) and the presenter view floats in front of
  it until the show ends; closing it leaves the show alone on the screen.
- A window opened without a position while the root is full screen goes on another monitor.

`DECKCRAFT_MONITORS="left,top,right,bottom;..."` (logical pixels) replaces the monitor list
and each window's monitor, so the placement can be tried on one screen split in two:
`DECKCRAFT_MONITORS="0,0,959,1079;960,0,1919,1079"` on a 1920 × 1080 desktop.

## Updates

DeckCraft is installed and updated as a package (pkgman, SoftwareUpdater). Upstream's release
check and downloads don't apply: the start-up check stays off, Check for Updates is left out of
the command palette, and the command itself (still reachable from the control channel) only says
that SoftwareUpdater installs newer versions. The community (Discord) links in About and File ›
Info stay; they open the web browser.

## Touchpad scrolling and input methods

air/OS's mouse add-on marks a touchpad's scroll gesture (`B_MOUSE_WHEEL_CHANGED` with
`be:device_subtype` 2, the unrounded `be:wheel_precise_delta_x/y` and `be:wheel_phase`). The host
sends those as egui trackpad scrolling in points (a wheel step is egui's line,
`InputOptions::line_scroll_speed`), with the gesture's start and end phases, so DeckCraft treats
it as the desktop app treats a trackpad: a zoomed-in slide follows the fingers, and at the fitted
size a flick turns one slide (with the trackpad's cooldown) while a resting finger turns none. A
mouse wheel (no precise deltas) still sends whole lines, one slide per notch. The end of a
gesture that arrives with its last movement is held for the next frame (egui drops the deltas of
a gesture that ended). There's no pinch gesture: Command+wheel and the zoom slider zoom.

The canvas view is `B_INPUT_METHOD_AWARE`: an input method (Japanese, Chinese, Korean…) composes
in the slide's text, as upstream's IME support does on the other desktops. `CHANGED` is the
pre-edit (its `be:selection` the active clause), `CHANGED` with `be:confirmed` the commit, and
`STOPPED` without one drops the pre-edit. A `LOCATION_REQUEST` is answered with the caret
(egui's `IMEOutput::cursor_rect`, on the screen) for each character, so the candidate window
opens by it. When the editor settles a composition itself (a click elsewhere, a key, a command)
it asks for an IME reset (`IMEOutput::reset_request`): the host sends the input method
`B_INPUT_METHOD_STOPPED`, ignores what it sends until it starts again, and then reports the reset
done (`Event::ImeResetCompleted`) after the input queued before it; until then the editor ignores
the input method. With nothing to type into (no `IMEOutput`) the input method is stopped too.

## Clipboard and opening files

Paste takes, in order: text (with HTML), an archived `BBitmap` ("image/bitmap", or
"image/x-be-bitmap") as ShowImage, Screenshot, WonderBrush and airShot copy, PNG, then JPEG, GIF,
WebP, BMP or TIFF data. Command+V with only a picture on the clipboard inserts it (the ribbon's
Paste, as upstream, pastes what DeckCraft itself copied). Copying shapes without text (a picture, a drawing) puts the editor's object marker
text on the clipboard, as upstream does, and a picture of the shapes (drawn as Paste Special ›
Picture draws them) as PNG and as an archived `BBitmap`, so they paste into Haiku programs; a
paste back into DeckCraft still pastes the shapes.

Of upstream's asynchronous `Services` hooks only `inbox` is set: files dropped on the window are
read on a thread and handed to the editor there (a presentation opens; a picture, sound or movie
is inserted on the slide shown when it was dropped, as the desktop app's drops are). `open_async`
and `download` are the web build's stand-ins for file panels; the editor uses them only when
`pick_open` / `pick_save` are missing, and it already opens a presentation picked in the Open
panel on a thread with a progress window (`open_path_in_background`). Presentations from
Tracker, the command line and drops open the same way, one after the other, so a large deck
doesn't stop the window. A PowerPoint Show handed over last, and `--show`, still open at once to
start the slide show.

DeckCraft is `B_SINGLE_LAUNCH` (`DeckCraft.rdef`): one copy runs from each executable file.
Presentations opened in Tracker while it runs (double-click, Open With, drops on its icon) arrive
in the running copy as `B_REFS_RECEIVED`, and a launch from Terminal (`DeckCraft talk.pptx`)
hands its arguments over (`B_ARGV_RECEIVED`, relative paths resolved against the launch's folder)
and ends before it touches the log file, the preferences or the recovery records
(`BApplication` reports `B_ALREADY_RUNNING`). They take the same path as Tracker's: in the
background, a PowerPoint Show last starting its slide show, and `--show` opening its
presentations at once and starting the show; other options (`--sample`, `--control`) are ignored.
The window comes to the front, un-minimized, also for a launch without presentations (the
Deskbar's, `B_SILENT_RELAUNCH`, which `BApplication` alone answers by activating the team, leaving
a minimized window down). Closing or quitting asks
about every presentation with unsaved changes, as before. A test copy that must run beside the
installed one runs from a file of its own (copies from different files run side by side, each
keeping its own recovery records), and a test build gets single launch only with the resources
(`rc`/`xres` of `DeckCraft.rdef`, as `tools/haiku/package.sh` adds them).

The window's frame and content size are saved in `~/config/settings/DeckCraft/window.json` when
DeckCraft quits (the last frame that wasn't minimized or a slide show) and restored at the next
start onto the monitor holding most of it (air/OS's `get_display_frame`, else the nearest one),
shrunk to fit and moved in until its tab and borders show. `DECKCRAFT_NO_PREFS` neither reads
nor writes it.

## Initial limitations

- Slides are still rendered by the engine on the CPU; only the interface is drawn with Vulkan.
- The host does not expose native accessibility nodes.
- This is the upstream alpha editor: its remaining format and editing limitations still apply.

## Revision history

- 2026-10-30: printing through the print kit (#1; Printing).
- 2026-10-30: every egui viewport in a `BWindow` of its own, the presenter view on a second
  monitor and on by default (#6; Windows, the slide show and the presenter view).
- 2026-10-29: one running copy; Tracker's opens and later launches go to it, `--show` and a
  PowerPoint Show starting the slide show there (#2; Clipboard and opening files).
- 2026-10-29: input methods compose in the slide's text (#5; Touchpad scrolling and input
  methods). `tools/haiku/input-probe.cpp` (built on air/OS with
  `g++ -o input-probe input-probe.cpp -lbe`) sends a running copy what a test machine can't
  produce: an input method's composition, pointer moves, touchpad gestures and wheel turns, and
  opens files with a given copy as Tracker does.
- 2026-10-29: a touchpad's scrolling is a trackpad's, a wheel's stays lines (#3; Touchpad
  scrolling).
- 2026-10-29: Check for Updates says SoftwareUpdater updates the package (#4; Updates).
- 2026-10-29: archived `BBitmap`s on the clipboard both ways, files dropped on the window opened
  or inserted through the editor's inbox, Tracker and command-line presentations opened in the
  background, and the window's place remembered (#8; Clipboard and opening files).
- 2026-10-29: movies (#7). DeckCraft's own H.264/HEVC/VP9/AV1 decoders already played movies on
  air/OS; the formats they don't read (Motion JPEG, MPEG-1/2, MPEG-4 Part 2, WMV, Theora, VP8,
  and WMA, MP2 and MP3-in-AVI audio) now play through the Media Kit (Movies).
- 2026-10-29: package `0.12.0~airos1-2` draws the window with Vulkan (Rendering). Framework 13
  (Radeon 760M, 175 %, the sample deck, 30 samples each, against the rasteriser in the same
  build): DeckCraft CPU 1.78 s -> 0.62 s, app_server 0.12 -> 0.11 s; hover 6.1 -> 5.2 ms and
  slide change 19.9 -> 17.2 ms per request.
- 2026-10-29: merged upstream DeckCraft 0.12.0 (storytold/deckcraft f893f6af); package
  `0.12.0~airos1-1`. The host follows egui 0.37 (screenshot callbacks, wheel source,
  `TextSelectionSettled`, the context-taking `raw_input_hook`), installs the IME pass hooks,
  and opens Tracker and command-line files through `open_from_system`, so a `.ppsx` opened
  last starts its slide show. Upstream's self-update checks stay off: pkgman updates the package.
- 2026-10-18: package revision 4 supplies the executable MIME type so files opened
  from Tracker launch directly into the editor. Test the actual Tracker launch;
  passing a filename on the command line or sending refs to an existing process
  does not exercise application pre-registration.
- 2026-10-18: initial Be API port, cross-build and package tooling.

## Native application icon (2026-10-26)

The air/OS icon is an original presentation screen and tripod, licensed under
MIT OR Apache-2.0. Its 64-unit vector drawing follows the
[Haiku icon guidelines](https://www.haiku-os.org/development/icon-guidelines):
vertical edges stay upright in perspective, the main light comes from the top
left, the right faces are darker, a floor shadow extends to the right, and the
silhouette has a two-unit black outline (one pixel at 32px). Broad details are
intended to remain legible at 16px and 32px. No fonts or third-party artwork are used.

Rebuild both the native HVIF and its SVG preview from this checkout:

```sh
python3 tools/haiku/make-icon.py
```

`apps/deckcraft-haiku/resources/DeckCraft.hvif` is the packaged icon;
`apps/deckcraft-haiku/resources/DeckCraft.svg` exports the same geometry for inspection.
The existing `.rdef` imports the HVIF, and `tools/haiku/package.sh` embeds it in
the executable's resources. Tracker and Deskbar therefore use the vector icon.
The upstream icons for other platforms remain separate. Native previews can be
rendered on Haiku with `BIconUtils::GetVectorIcon`; review at 16, 32, 64 and 128px
on light and dark backgrounds before changing the drawing.

Package version `0.7.0~airos1-5` includes this icon update.
