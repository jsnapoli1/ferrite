//! Server-side recording of a call: every participant's audio (Opus) and best video layer
//! (VP8) go into one WebM file per call session as the frames pass through the SFU.
//!
//! Timestamps come from each stream's RTP clock, anchored to the frame's arrival time on its
//! first frame, so audio and video stay smooth within a file and roughly aligned across files
//! (people who join later start later). A stream is re-anchored when its clock jumps (or the
//! video switches simulcast layers), and a participant whose file can't be written is
//! dropped from the recording rather than stopping it.

use crate::webm::WebmWriter;
use crate::{LAYERS, LayerPicker, Pick};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use str0m::format::Codec;
use str0m::media::{MediaData, Rid};
use tracing::warn;

/// A finished recording of one call session.
#[derive(Debug, Clone)]
pub struct RecordedFile {
    pub session: String,
    pub path: PathBuf,
    pub duration_ms: u64,
    pub bytes: u64,
    pub video_frames: u64,
    pub audio_frames: u64,
    pub dimensions: Option<(u32, u32)>,
}

/// Re-anchor when RTP-derived time and arrival time disagree by more than this (ms).
const MAX_DRIFT_MS: i64 = 2_000;

#[derive(Clone, Copy)]
struct Anchor {
    rtp_ms: i64,
    ms: i64,
}

fn stamp(anchor: &mut Option<Anchor>, rtp_ms: i64, now_ms: i64) -> i64 {
    if let Some(a) = anchor {
        let ms = a.ms + (rtp_ms - a.rtp_ms);
        if (ms - now_ms).abs() < MAX_DRIFT_MS {
            return ms;
        }
    }
    *anchor = Some(Anchor { rtp_ms, ms: now_ms });
    now_ms
}

struct Participant {
    writer: Option<WebmWriter>,
    path: PathBuf,
    video: LayerPicker,
    video_rid: Option<Rid>,
    video_anchor: Option<Anchor>,
    audio_anchor: Option<Anchor>,
}

pub(crate) struct RoomRecording {
    dir: PathBuf,
    started: Instant,
    participants: HashMap<String, Participant>,
}

impl RoomRecording {
    pub(crate) fn new(dir: PathBuf) -> std::io::Result<RoomRecording> {
        std::fs::create_dir_all(&dir)?;
        Ok(RoomRecording {
            dir,
            started: Instant::now(),
            participants: HashMap::new(),
        })
    }

    /// Records one frame from `session`. Returns a simulcast layer whose keyframe the
    /// recording is waiting for (the caller asks the sender for it).
    pub(crate) fn write(&mut self, session: &str, data: &MediaData) -> Option<Rid> {
        let codec = data.params.spec().codec;
        if !matches!(codec, Codec::Opus | Codec::Vp8) {
            return None;
        }
        let p = match self.participants.get_mut(session) {
            Some(p) => p,
            None => {
                let path = self.dir.join(format!("{session}.webm"));
                // WebRTC declares Opus as two channels (opus/48000/2) even for mono content.
                let writer = match WebmWriter::create(&path, session, 2) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        warn!(session, error = %e, "cannot create recording file");
                        None
                    }
                };
                self.participants.entry(session.to_string()).or_insert(Participant {
                    writer,
                    path,
                    video: LayerPicker::default(),
                    video_rid: None,
                    video_anchor: None,
                    audio_anchor: None,
                })
            }
        };
        let writer = p.writer.as_mut()?;
        let now_ms = data.network_time.saturating_duration_since(self.started).as_millis() as i64;
        let rtp_ms = (data.time.numer() as i128 * 1000 / data.time.denom().max(1) as i128) as i64;
        let result = if codec == Codec::Opus {
            let ms = stamp(&mut p.audio_anchor, rtp_ms, now_ms);
            writer.audio(ms, &data.data)
        } else {
            match p.video.pick(data, LAYERS.len() - 1) {
                Pick::Skip(keyframe) => return keyframe,
                Pick::Pass => {}
            }
            // Layers may use different RTP clocks: start a new anchor on a switch.
            if p.video_rid != data.rid {
                p.video_rid = data.rid;
                p.video_anchor = None;
            }
            let ms = stamp(&mut p.video_anchor, rtp_ms, now_ms);
            writer.video(ms, data.is_keyframe(), &data.data)
        };
        if let Err(e) = result {
            warn!(session, error = %e, "recording write failed; this participant's file stops here");
            p.writer = None;
        }
        None
    }

    /// Finalizes every file. Files that never got a frame are removed.
    pub(crate) fn finish(self) -> Vec<RecordedFile> {
        let mut out = Vec::new();
        for (session, p) in self.participants {
            let Some(writer) = p.writer else { continue };
            match writer.finish() {
                Ok(done) if done.audio_frames + done.video_frames > 0 => out.push(RecordedFile {
                    session,
                    path: p.path,
                    duration_ms: done.duration_ms,
                    bytes: done.bytes,
                    video_frames: done.video_frames,
                    audio_frames: done.audio_frames,
                    dimensions: done.dimensions,
                }),
                Ok(_) => {
                    let _ = std::fs::remove_file(&p.path);
                }
                Err(e) => warn!(session, error = %e, "finishing recording file failed"),
            }
        }
        out.sort_by(|a, b| a.session.cmp(&b.session));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_follow_rtp_time_and_reanchor_on_jumps() {
        let mut a = None;
        assert_eq!(stamp(&mut a, 10_000, 50), 50);
        // RTP time governs (smooth even if arrival jitters)…
        assert_eq!(stamp(&mut a, 10_020, 95), 70);
        assert_eq!(stamp(&mut a, 10_040, 88), 90);
        // …until it disagrees with arrival by seconds (a restarted stream): re-anchor.
        assert_eq!(stamp(&mut a, 500, 120), 120);
        assert_eq!(stamp(&mut a, 520, 140), 140);
    }
}
