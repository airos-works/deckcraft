//! The system's own codecs, for media DeckCraft's decoders don't read (Motion JPEG, MPEG-1/2,
//! MPEG-4 Part 2, Windows Media, Theora, VP8…). A host may register one [`Platform`] when it
//! starts (the air/OS host: the Media Kit). Probing, audio decoding and video decoding try
//! DeckCraft's own code first and ask the platform only for what that couldn't read, so a file
//! DeckCraft decodes itself plays the same everywhere.

use std::sync::OnceLock;

use crate::{Bytes, Frame, MediaInfo, Pcm, Result};

/// Decoded video frames, in presentation order, as [`crate::VideoDecoder`] gives them.
pub trait VideoSource: Send {
    /// Width and height in pixels.
    fn size(&self) -> (u32, u32);
    /// The next frame, `None` at the end.
    fn next_frame(&mut self) -> Result<Option<Frame>>;
    /// Position so that the next frame is the one shown at `t` seconds.
    fn seek(&mut self, t: f64);
}

/// Codecs a host offers. Each answers `None` for media it doesn't read either.
pub trait Platform: Send + Sync {
    /// The file's streams; `decodable` where the platform can decode one.
    fn probe(&self, bytes: &Bytes) -> Option<MediaInfo>;
    /// The audio of an audio file or a video, as [`crate::audio::decode`] gives it.
    fn decode_audio(&self, bytes: &Bytes) -> Option<Result<Pcm>>;
    /// The video track's decoder.
    fn open_video(&self, bytes: &Bytes) -> Option<Result<Box<dyn VideoSource>>>;
}

static PLATFORM: OnceLock<Box<dyn Platform>> = OnceLock::new();

/// Register the host's codecs, once, before any media is read. `false` when one already was.
pub fn set(platform: Box<dyn Platform>) -> bool {
    PLATFORM.set(platform).is_ok()
}

pub(crate) fn get() -> Option<&'static dyn Platform> {
    PLATFORM.get().map(|p| p.as_ref())
}

/// `own`, completed by what the platform says about the streams DeckCraft can't decode.
pub(crate) fn complete_probe(bytes: &Bytes, own: Result<MediaInfo>) -> Result<MediaInfo> {
    let needs = |info: &MediaInfo| {
        info.video.as_ref().is_some_and(|v| !v.decodable)
            || info.audio.as_ref().is_some_and(|a| !a.decodable)
            || info.video.is_none() && info.audio.is_none()
            // Containers we take only the audio from (Ogg Theora, MPEG program streams) can still
            // hold a video the platform reads. Not plain audio files: an MP3's cover art would
            // come back as a one-picture "video".
            || info.video.is_none() && matches!(info.container, "ogg" | "asf" | "unknown")
    };
    if own.as_ref().is_ok_and(|i| !needs(i)) {
        return own;
    }
    let Some(theirs) = get().and_then(|p| p.probe(bytes)) else { return own };
    let Ok(mut info) = own else { return Ok(theirs) };
    if theirs.video.as_ref().is_some_and(|v| v.decodable) && !info.video.as_ref().is_some_and(|v| v.decodable) {
        info.video = theirs.video;
    }
    if theirs.audio.as_ref().is_some_and(|a| a.decodable) && !info.audio.as_ref().is_some_and(|a| a.decodable) {
        info.audio = theirs.audio;
    }
    if info.duration_ms == 0 || info.container == "unknown" {
        info.duration_ms = theirs.duration_ms.max(info.duration_ms);
    }
    if info.container == "unknown" {
        info.container = theirs.container;
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use super::*;
    use crate::{AudioInfo, VideoInfo};

    /// Answers only for bytes starting `FAKEMOVIE`, so the other tests in this binary keep
    /// DeckCraft's own decoders.
    struct Fake;

    struct FakeVideo(u32);

    impl VideoSource for FakeVideo {
        fn size(&self) -> (u32, u32) {
            (2, 2)
        }
        fn next_frame(&mut self) -> Result<Option<Frame>> {
            self.0 += 1;
            Ok((self.0 <= 3).then(|| Frame { time: f64::from(self.0 - 1) / 10.0, width: 2, height: 2, rgba: vec![255; 16] }))
        }
        fn seek(&mut self, t: f64) {
            self.0 = (t * 10.0) as u32;
        }
    }

    fn fake(bytes: &Bytes) -> bool {
        bytes.starts_with(b"FAKEMOVIE")
    }

    impl Platform for Fake {
        fn probe(&self, bytes: &Bytes) -> Option<MediaInfo> {
            fake(bytes).then(|| MediaInfo {
                container: "fake",
                duration_ms: 300,
                audio: Some(AudioInfo { codec: "Fake".into(), sample_rate: 8000, channels: 1, decodable: true }),
                video: Some(VideoInfo { codec: "Fake".into(), width: 2, height: 2, fps: 10.0, frames: 3, decodable: true }),
            })
        }
        fn decode_audio(&self, bytes: &Bytes) -> Option<Result<Pcm>> {
            fake(bytes).then(|| Ok(Pcm { rate: 8000, channels: 1, samples: vec![0.0; 2400] }))
        }
        fn open_video(&self, bytes: &Bytes) -> Option<Result<Box<dyn VideoSource>>> {
            fake(bytes).then(|| Ok(Box::new(FakeVideo(0)) as Box<dyn VideoSource>))
        }
    }

    #[test]
    fn the_platform_reads_only_what_deckcraft_cannot() {
        set(Box::new(Fake));
        let movie: Bytes = Arc::new(b"FAKEMOVIE and then some".to_vec());
        let info = crate::probe(&movie).expect("probed by the platform");
        assert_eq!((info.container, info.duration_ms), ("fake", 300));
        assert!(info.playable());
        assert_eq!(crate::audio::decode(&movie).expect("platform audio").frames(), 2400);
        let mut feed = crate::VideoFeed::new(movie.clone()).expect("platform video");
        assert_eq!(feed.size(), (2, 2));
        let f = feed.frame_at_blocking(0.15, std::time::Duration::from_secs(5)).expect("a frame");
        assert!((f.time - 0.1).abs() < 1e-9);
        assert_eq!(crate::poster_frame(&movie, 0.0).expect("platform poster").width, 2);

        // Anything else is untouched: unreadable bytes stay unreadable.
        let junk: Bytes = Arc::new(b"not media at all".to_vec());
        assert!(crate::probe(&junk).is_err());
        assert!(crate::VideoFeed::new(junk).is_err());
    }
}
