//! The Media Kit as DeckCraft's codecs for the movies and sounds its own decoders don't read
//! (`deckcraft_media::platform`): Motion JPEG, MPEG-1/2, MPEG-4 Part 2 (DivX/Xvid), Windows
//! Media Video and Audio, Theora, VP8 and whatever else the system's ffmpeg plugin decodes, in
//! AVI, ASF/WMV, MPEG, Ogg, QuickTime and Matroska. H.264, HEVC, VP9 and AV1 in MP4/MOV/WebM
//! are DeckCraft's own decoders' too, but the Media Kit decodes them first where it can
//! ([`MediaKit::open_video_first`]): with air/OS's hardware decoder add-ons (NVDEC, VA-API,
//! MPP…) or the ffmpeg add-on's libavcodec and dav1d, which decode 1080p three to five times
//! faster than DeckCraft's own decoders on air/OS (docs/haiku.md, Movies).
//! `DECKCRAFT_SYSTEM_VIDEO=hardware` keeps that to hardware decoders, `0` to none. Frames go
//! into the same video feed and texture as theirs, timed by the same clock (the audio
//! callback's), so play, pause, seek, loop and the movie's controls behave the same.

use std::sync::OnceLock;
use std::time::Instant;

use deckcraft_media::{AudioInfo, Bytes, Frame, MediaError, MediaInfo, Pcm, Platform, VideoDecoder, VideoInfo, VideoSource};

use crate::host::{self, MediaKitVideo};

/// As much decoded audio as DeckCraft's own decoders keep (about 6 minutes of 48 kHz 5.1).
const MAX_SAMPLES: usize = 64 * 1024 * 1024;

/// A video that gives up after this many frames in a row fail to decode.
const MAX_ERRORS: u32 = 50;

pub struct MediaKit;

/// Offer the Media Kit's codecs to the app, unless `DECKCRAFT_NO_MEDIA_KIT` is set.
pub fn install() {
    if std::env::var_os("DECKCRAFT_NO_MEDIA_KIT").is_none() && !deckcraft_media::platform::set(Box::new(MediaKit)) {
        log::warn!("media: codecs were already registered");
    }
}

/// The ffmpeg reader's name for a container ("mov,mp4,m4a,3gp,3g2,mj2") as the probe's.
fn container(name: &str) -> &'static str {
    match name.split(',').next().unwrap_or_default() {
        "avi" => "avi",
        "asf" => "asf",
        "mpeg" | "mpegvideo" | "mpegts" => "mpeg",
        "ogg" => "ogg",
        "mov" => "mov",
        "matroska" => "matroska",
        "flv" => "flv",
        "dv" => "dv",
        "rm" => "realmedia",
        "wav" => "wav",
        "aiff" => "aiff",
        "mp3" => "mp3",
        _ => "media",
    }
}

/// The track's frame count, or the duration's worth at the frame rate where the reader's count
/// can't be right (its WebM reader gives hundreds of millions for a few seconds).
fn frame_count(frames: i64, duration_us: i64, fps: f64) -> u64 {
    let expected = duration_us as f64 / 1e6 * fps;
    let frames = u64::try_from(frames).unwrap_or(0);
    if expected > 0.0 && (frames as f64 > expected * 1.5 + 10.0 || frames == 0) { expected.round() as u64 } else { frames }
}

impl Platform for MediaKit {
    fn probe(&self, bytes: &Bytes) -> Option<MediaInfo> {
        let info = host::media_probe(bytes)?;
        if info.has_video == 0 && info.has_audio == 0 {
            return None;
        }
        let fps = f64::from(info.fps);
        Some(MediaInfo {
            container: container(&info.container()),
            duration_ms: u64::try_from(info.duration_us / 1000).unwrap_or(0),
            audio: (info.has_audio != 0).then(|| AudioInfo {
                codec: info.audio_codec(),
                sample_rate: if info.rate.is_finite() && info.rate > 0.0 { info.rate.round() as u32 } else { 0 },
                channels: u16::try_from(info.channels).unwrap_or(0),
                decodable: info.audio_decodable != 0,
            }),
            video: (info.has_video != 0).then(|| VideoInfo {
                codec: info.video_codec(),
                width: u32::try_from(info.width).unwrap_or(0),
                height: u32::try_from(info.height).unwrap_or(0),
                fps: if fps.is_finite() && fps > 0.0 { fps } else { 0.0 },
                frames: frame_count(info.frames, info.duration_us, fps),
                decodable: info.video_decodable != 0,
            }),
        })
    }

    fn decode_audio(&self, bytes: &Bytes) -> Option<deckcraft_media::Result<Pcm>> {
        let (channels, rate, samples) = host::media_audio(bytes, MAX_SAMPLES)?;
        Some(Ok(Pcm { rate, channels, samples }))
    }

    fn open_video(&self, bytes: &Bytes) -> Option<deckcraft_media::Result<Box<dyn VideoSource>>> {
        let dec = MediaKitVideo::open(bytes.clone())?;
        log::info!("video: {}x{} {} through the Media Kit ({})", dec.width, dec.height, dec.container, dec.decoder);
        Some(Ok(Box::new(KitVideo::new(dec, bytes))))
    }

    /// The Media Kit first wherever it decodes the movie: on a hardware decoder (NVDEC takes
    /// H.264 on NVIDIA cards, MPP H.264/HEVC/VP9/AV1 on the RK3588…), the frames come off the
    /// graphics card; otherwise libavcodec and dav1d decode on the processor, several times
    /// faster than DeckCraft's own decoders here. A movie the system's decoder then refuses
    /// before its first frame goes back to DeckCraft's own decoder.
    fn open_video_first(&self, bytes: &Bytes) -> Option<deckcraft_media::Result<Box<dyn VideoSource>>> {
        let policy = system_video();
        if policy == SystemVideo::Off {
            return None;
        }
        let dec = MediaKitVideo::open(bytes.clone())?;
        if policy == SystemVideo::Hardware && !dec.hardware {
            return None;
        }
        log::info!(
            "video: {}x{} {} decoded by the Media Kit's {} ({})",
            dec.width,
            dec.height,
            dec.container,
            dec.decoder,
            if dec.hardware { "hardware" } else { "processor" }
        );
        Some(Ok(Box::new(SystemFirst { kit: Some(KitVideo::new(dec, bytes)), own: None, bytes: bytes.clone(), shown: false, seek: None })))
    }
}

/// Which movies DeckCraft also decodes itself go to the Media Kit first (`DECKCRAFT_SYSTEM_VIDEO`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SystemVideo {
    /// Every one it decodes (the default).
    All,
    /// Those it decodes on a hardware decoder add-on (`hardware`).
    Hardware,
    /// None (`0`): DeckCraft's decoders, as on the other platforms.
    Off,
}

fn system_video() -> SystemVideo {
    static POLICY: OnceLock<SystemVideo> = OnceLock::new();
    *POLICY.get_or_init(|| {
        let policy = match std::env::var("DECKCRAFT_SYSTEM_VIDEO").as_deref() {
            Ok("0" | "off" | "none") => SystemVideo::Off,
            Ok("hardware") => SystemVideo::Hardware,
            _ => SystemVideo::All,
        };
        log::info!("video: the Media Kit first for {policy:?} movies (hardware decoder add-on installed: {})", host::hardware_video_installed());
        policy
    })
}

/// The Media Kit's decoder, and DeckCraft's own in its place when the Media Kit refuses the
/// movie before giving a frame (a hardware add-on that takes a format refuses what it can't
/// decode only then: NVDEC, for one, H.264 fields or 4:2:2).
struct SystemFirst {
    kit: Option<KitVideo>,
    own: Option<VideoDecoder>,
    bytes: Bytes,
    /// The hardware gave a frame: its errors are the movie's from now on.
    shown: bool,
    /// The last seek, for the decoder taking over.
    seek: Option<f64>,
}

impl SystemFirst {
    fn take_over(&mut self, why: &str) -> deckcraft_media::Result<Option<Frame>> {
        log::warn!("video: the Media Kit's decoder {why}; DeckCraft decodes the movie");
        self.kit = None;
        let mut own = VideoDecoder::open(self.bytes.clone())?;
        if let Some(t) = self.seek {
            own.seek(t);
        }
        let frame = own.next_frame();
        self.own = Some(own);
        frame
    }
}

impl VideoSource for SystemFirst {
    fn size(&self) -> (u32, u32) {
        match (&self.kit, &self.own) {
            (Some(kit), _) => kit.size(),
            (None, Some(own)) => own.size(),
            (None, None) => (0, 0),
        }
    }

    fn next_frame(&mut self) -> deckcraft_media::Result<Option<Frame>> {
        if let Some(own) = &mut self.own {
            return own.next_frame();
        }
        let Some(kit) = &mut self.kit else { return Ok(None) };
        match kit.next_frame() {
            Ok(Some(f)) => {
                self.shown = true;
                Ok(Some(f))
            }
            Ok(None) if !self.shown => self.take_over("gave no picture"),
            Err(e) if !self.shown => self.take_over(&e.to_string()),
            other => other,
        }
    }

    fn seek(&mut self, t: f64) {
        self.seek = Some(t);
        if let Some(own) = &mut self.own {
            own.seek(t);
        } else if let Some(kit) = &mut self.kit {
            kit.seek(t);
        }
    }
}

/// `DeckCraft --bench-video FILE…`: decodes each movie with DeckCraft's own decoder and through
/// the Media Kit, and prints frames, time and processor time for each.
pub fn bench(files: &[String]) {
    for file in files {
        let bytes: Bytes = match std::fs::read(file) {
            Ok(b) => std::sync::Arc::new(b),
            Err(e) => {
                println!("{file}: {e}");
                continue;
            }
        };
        let own = VideoDecoder::open(bytes.clone()).map(|d| Box::new(d) as Box<dyn VideoSource>).map_err(|e| e.to_string());
        let kit = MediaKitVideo::open(bytes.clone()).map(|d| {
            let name = format!("Media Kit ({}{})", d.decoder, if d.hardware { ", hardware" } else { "" });
            (name, Box::new(KitVideo::new(d, &bytes)) as Box<dyn VideoSource>)
        });
        let mut sample: Vec<Frame> = Vec::new();
        for (name, source) in
            [("DeckCraft".to_string(), own), kit.map_or_else(|| ("Media Kit".to_string(), Err("no decoder".into())), |(n, s)| (n, Ok(s)))]
        {
            let mut source = match source {
                Ok(s) => s,
                Err(e) => {
                    println!("{file}: {name}: {e}");
                    continue;
                }
            };
            let (start, cpu) = (Instant::now(), host::team_cpu_time());
            let mut frames = 0u32;
            let mut failed = None;
            while frames < 600 {
                match source.next_frame() {
                    Ok(Some(f)) => {
                        frames += 1;
                        if frames == 60 {
                            sample.push(f);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            let (wall, cpu) = (start.elapsed().as_secs_f64(), (host::team_cpu_time().saturating_sub(cpu)).as_secs_f64());
            let (w, h) = source.size();
            println!(
                "{file}: {name}: {w}x{h}, {frames} frames in {wall:.2} s ({:.1} a second), processor {cpu:.2} s ({:.1} ms a frame){}",
                f64::from(frames) / wall.max(1e-9),
                cpu * 1e3 / f64::from(frames.max(1)),
                failed.map(|e| format!(", stopped: {e}")).unwrap_or_default()
            );
        }
        // The two decoders' 60th pictures: how far apart they are, on average and at most.
        if let [a, b] = sample.as_slice()
            && a.rgba.len() == b.rgba.len()
        {
            let (sum, max) = a.rgba.iter().zip(&b.rgba).fold((0u64, 0u8), |(s, m), (x, y)| (s + u64::from(x.abs_diff(*y)), m.max(x.abs_diff(*y))));
            println!("{file}: 60th pictures differ by {:.2} on average, {max} at most (of 255)", sum as f64 / a.rgba.len().max(1) as f64);
        }
    }
}

/// Frames from the Media Kit in presentation order. The Media Kit seeks to key frames; the
/// frames between one and the time asked for are decoded and dropped here, keeping the last one
/// before it, as DeckCraft's own decoder does.
struct KitVideo {
    dec: MediaKitVideo,
    bytes: Bytes,
    /// The reader only estimates where a time is (Ogg, MPEG program streams): it lands Theora on
    /// a frame that isn't a key frame (the old picture with new changes drawn over it) and MPEG
    /// after the time asked for. Seeking there decodes from the start instead.
    estimated_seeks: bool,
    /// Frames before this time (seconds) are not shown (after a seek).
    skip_before: f64,
    /// The latest frame before `skip_before`: on screen at that time if no frame starts there.
    held: Option<Frame>,
    /// A frame read past `held`, given out next.
    pending: Option<Frame>,
}

impl KitVideo {
    fn new(dec: MediaKitVideo, bytes: &Bytes) -> Self {
        let estimated_seeks = matches!(container(&dec.container), "ogg" | "mpeg");
        KitVideo { dec, bytes: bytes.clone(), estimated_seeks, skip_before: f64::NEG_INFINITY, held: None, pending: None }
    }
}

impl VideoSource for KitVideo {
    fn size(&self) -> (u32, u32) {
        (self.dec.width, self.dec.height)
    }

    fn next_frame(&mut self) -> deckcraft_media::Result<Option<Frame>> {
        if let Some(f) = self.pending.take() {
            return Ok(Some(f));
        }
        let mut errors = 0;
        loop {
            let mut rgba = Vec::new();
            match self.dec.next(&mut rgba) {
                Ok(Some(us)) => {
                    let f = Frame { time: us as f64 / 1e6, width: self.dec.width, height: self.dec.height, rgba };
                    if f.time + 1e-6 < self.skip_before {
                        self.held = Some(f);
                        continue;
                    }
                    if let Some(h) = self.held.take()
                        && f.time > self.skip_before + 1e-6
                    {
                        self.pending = Some(f);
                        return Ok(Some(h));
                    }
                    return Ok(Some(f));
                }
                // The end while skipping: the last frame is the one on screen.
                Ok(None) => return Ok(self.held.take()),
                Err(()) => {
                    // A frame lost to a damaged sample: carry on, but not for ever.
                    errors += 1;
                    if errors > MAX_ERRORS {
                        return Err(MediaError::Corrupt("the Media Kit couldn't decode the video".into()));
                    }
                }
            }
        }
    }

    fn seek(&mut self, t: f64) {
        let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
        self.held = None;
        self.pending = None;
        self.skip_before = t;
        let target = (t * 1e6) as i64;
        if !self.estimated_seeks && self.dec.seek(target) <= target {
            return;
        }
        // From the start: a fresh decoder, so no picture of the old position leaks into the new.
        match MediaKitVideo::open(self.bytes.clone()) {
            Some(dec) => self.dec = dec,
            None => {
                self.dec.seek(0);
            }
        }
    }
}
