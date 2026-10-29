//! The Media Kit as DeckCraft's codecs for the movies and sounds its own decoders don't read
//! (`deckcraft_media::platform`): Motion JPEG, MPEG-1/2, MPEG-4 Part 2 (DivX/Xvid), Windows
//! Media Video and Audio, Theora, VP8 and whatever else the system's ffmpeg plugin decodes, in
//! AVI, ASF/WMV, MPEG, Ogg, QuickTime and Matroska. H.264, HEVC, VP9 and AV1 in MP4/MOV/WebM
//! stay with DeckCraft's own decoders. Frames go into the same video feed and texture as theirs,
//! timed by the same clock (the audio callback's), so play, pause, seek, loop and the movie's
//! controls behave the same.

use deckcraft_media::{AudioInfo, Bytes, Frame, MediaError, MediaInfo, Pcm, Platform, VideoInfo, VideoSource};

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
        log::info!("video: {}x{} {} through the Media Kit", dec.width, dec.height, dec.container);
        let estimated_seeks = matches!(container(&dec.container), "ogg" | "mpeg");
        Some(Ok(Box::new(KitVideo { dec, bytes: bytes.clone(), estimated_seeks, skip_before: f64::NEG_INFINITY, held: None, pending: None })))
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
