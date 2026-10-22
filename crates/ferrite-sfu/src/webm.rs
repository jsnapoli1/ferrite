//! A small WebM (Matroska) writer for call recordings. Frames go in exactly as the SFU
//! receives them (VP8 video, Opus audio), so recording costs no decoding or encoding.
//!
//! Layout: EBML header, then one Segment holding a SeekHead (reserved up front, filled in on
//! finish), Info, Tracks, Clusters and Cues. Clusters are buffered in memory and written with
//! their size known; a new one starts at a video keyframe (or every few seconds), and each
//! keyframe cluster gets a cue point so players can seek. `finish` patches the duration, the
//! video dimensions and the Segment size, which were unknown while recording.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

const EBML: u32 = 0x1A45_DFA3;
const EBML_VERSION: u32 = 0x4286;
const EBML_READ_VERSION: u32 = 0x42F7;
const EBML_MAX_ID_LENGTH: u32 = 0x42F2;
const EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
const DOC_TYPE: u32 = 0x4282;
const DOC_TYPE_VERSION: u32 = 0x4287;
const DOC_TYPE_READ_VERSION: u32 = 0x4285;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2AD7B1;
const DURATION: u32 = 0x4489;
const MUXING_APP: u32 = 0x4D80;
const WRITING_APP: u32 = 0x5741;
const TITLE: u32 = 0x7BA9;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_UID: u32 = 0x73C5;
const TRACK_TYPE: u32 = 0x83;
const FLAG_LACING: u32 = 0x9C;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const SEEK_PRE_ROLL: u32 = 0x56BB;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const CUES: u32 = 0x1C53_BB6B;
const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;
const VOID: u32 = 0xEC;

pub const VIDEO_TRACK: u64 = 1;
pub const AUDIO_TRACK: u64 = 2;

/// Space kept at the start of the Segment for the SeekHead written by `finish`.
const SEEK_HEAD_RESERVED: usize = 96;
/// Start a new cluster at a video keyframe once the current one is this old (ms)…
const CLUSTER_MIN_MS: i64 = 1_000;
/// …and in any case once it is this old (audio-only files, long keyframe intervals).
const CLUSTER_MAX_MS: i64 = 5_000;

fn id_bytes(id: u32) -> Vec<u8> {
    let b = id.to_be_bytes();
    let skip = b.iter().position(|&x| x != 0).unwrap_or(3);
    b[skip..].to_vec()
}

/// EBML variable-size integer with the shortest length.
fn vint(n: u64) -> Vec<u8> {
    for len in 1..=8u32 {
        // All-ones is reserved (unknown size), so the largest value per length is 2^(7·len) - 2.
        if n < (1u64 << (7 * len)) - 1 {
            let marked = n | (1u64 << (7 * len));
            return marked.to_be_bytes()[8 - len as usize..].to_vec();
        }
    }
    panic!("EBML size too large");
}

/// An 8-byte EBML size, so it can be patched in place later.
fn vint8(n: u64) -> [u8; 8] {
    let mut b = n.to_be_bytes();
    b[0] = 0x01;
    b
}

fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = id_bytes(id);
    out.extend(vint(body.len() as u64));
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    let b = v.to_be_bytes();
    let skip = b.iter().position(|&x| x != 0).unwrap_or(7);
    element(id, &b[skip..])
}

/// An unsigned integer in a fixed 8-byte body (patchable).
fn uint8(id: u32, v: u64) -> Vec<u8> {
    element(id, &v.to_be_bytes())
}

fn float(id: u32, v: f64) -> Vec<u8> {
    element(id, &v.to_be_bytes())
}

fn string(id: u32, v: &str) -> Vec<u8> {
    element(id, v.as_bytes())
}

/// A Void element of exactly `total` bytes (at least 2).
fn void(total: usize) -> Vec<u8> {
    assert!(total >= 2);
    // One size byte up to 127 body bytes, else an 8-byte size.
    if total - 2 < 127 {
        element(VOID, &vec![0; total - 2])
    } else {
        let mut out = id_bytes(VOID);
        out.extend(vint8((total - 9) as u64));
        out.extend(vec![0; total - 9]);
        out
    }
}

/// Width and height from a VP8 keyframe header (RFC 6386 §9.1).
pub fn vp8_dimensions(frame: &[u8]) -> Option<(u32, u32)> {
    if frame.len() < 10 || frame[0] & 1 != 0 || frame[3..6] != [0x9D, 0x01, 0x2A] {
        return None;
    }
    let w = u16::from_le_bytes([frame[6], frame[7]]) & 0x3FFF;
    let h = u16::from_le_bytes([frame[8], frame[9]]) & 0x3FFF;
    Some((w as u32, h as u32))
}

struct Block {
    track: u64,
    ms: i64,
    keyframe: bool,
    data: Vec<u8>,
}

pub struct WebmWriter {
    out: BufWriter<File>,
    /// File offset where the Segment's body starts (positions in SeekHead/Cues are relative to it).
    segment_body: u64,
    segment_size_pos: u64,
    seek_head_pos: u64,
    info_pos: u64,
    tracks_pos: u64,
    duration_pos: u64,
    width_pos: u64,
    height_pos: u64,
    dims: Option<(u32, u32)>,
    cluster: Vec<Block>,
    cues: Vec<(i64, u64, u64)>,
    end_ms: i64,
    /// Latest timestamp already written; later frames never go before it.
    flushed_ms: i64,
    pub video_frames: u64,
    pub audio_frames: u64,
}

impl WebmWriter {
    /// Creates `path` with an Opus track (`channels`) and a VP8 track.
    pub fn create(path: &Path, title: &str, channels: u8) -> io::Result<WebmWriter> {
        let mut out = BufWriter::new(File::create(path)?);
        let header = [
            uint(EBML_VERSION, 1),
            uint(EBML_READ_VERSION, 1),
            uint(EBML_MAX_ID_LENGTH, 4),
            uint(EBML_MAX_SIZE_LENGTH, 8),
            string(DOC_TYPE, "webm"),
            uint(DOC_TYPE_VERSION, 4),
            uint(DOC_TYPE_READ_VERSION, 2),
        ]
        .concat();
        out.write_all(&element(EBML, &header))?;
        out.write_all(&id_bytes(SEGMENT))?;
        let segment_size_pos = out.stream_position()?;
        // Unknown size until `finish`.
        out.write_all(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF])?;
        let segment_body = out.stream_position()?;
        let seek_head_pos = segment_body;
        out.write_all(&void(SEEK_HEAD_RESERVED))?;

        let info_pos = out.stream_position()?;
        let info_body = [
            uint(TIMESTAMP_SCALE, 1_000_000),
            string(MUXING_APP, "Ferrite"),
            string(WRITING_APP, "Ferrite"),
            string(TITLE, title),
        ]
        .concat();
        // Duration (float, patched later) goes last so its offset is easy to find.
        let mut info = id_bytes(INFO);
        let duration = float(DURATION, 0.0);
        info.extend(vint((info_body.len() + duration.len()) as u64));
        info.extend(&info_body);
        let duration_pos = info_pos + info.len() as u64 + (duration.len() - 8) as u64;
        info.extend(&duration);
        out.write_all(&info)?;

        let tracks_pos = out.stream_position()?;
        let mut opus_head = b"OpusHead".to_vec();
        opus_head.push(1); // version
        opus_head.push(channels);
        opus_head.extend(0u16.to_le_bytes()); // pre-skip: WebRTC streams start without one
        opus_head.extend(48_000u32.to_le_bytes());
        opus_head.extend(0i16.to_le_bytes()); // output gain
        opus_head.push(0); // channel mapping family
        let audio = element(
            TRACK_ENTRY,
            &[
                uint(TRACK_NUMBER, AUDIO_TRACK),
                uint(TRACK_UID, AUDIO_TRACK),
                uint(TRACK_TYPE, 2),
                uint(FLAG_LACING, 0),
                string(CODEC_ID, "A_OPUS"),
                element(CODEC_PRIVATE, &opus_head),
                uint(SEEK_PRE_ROLL, 80_000_000),
                element(AUDIO, &[float(SAMPLING_FREQUENCY, 48_000.0), uint(CHANNELS, channels as u64)].concat()),
            ]
            .concat(),
        );
        // Dimensions as fixed 8-byte integers so `finish` can write the real ones.
        let video_fields = [uint8(PIXEL_WIDTH, 640), uint8(PIXEL_HEIGHT, 360)].concat();
        let video_head = [
            uint(TRACK_NUMBER, VIDEO_TRACK),
            uint(TRACK_UID, VIDEO_TRACK),
            uint(TRACK_TYPE, 1),
            uint(FLAG_LACING, 0),
            string(CODEC_ID, "V_VP8"),
        ]
        .concat();
        let video_el = element(VIDEO, &video_fields);
        let video_body = [video_head.clone(), video_el.clone()].concat();
        let video = element(TRACK_ENTRY, &video_body);
        let tracks_body = [video.clone(), audio].concat();
        let mut tracks = id_bytes(TRACKS);
        tracks.extend(vint(tracks_body.len() as u64));
        let video_body_start = tracks_pos + tracks.len() as u64 + (video.len() - video_body.len()) as u64;
        let video_fields_start = video_body_start + video_head.len() as u64 + (video_el.len() - video_fields.len()) as u64;
        // Each field: 1-byte id + 1-byte size + 8-byte value.
        let width_pos = video_fields_start + 2;
        let height_pos = video_fields_start + 10 + 2;
        tracks.extend(&tracks_body);
        out.write_all(&tracks)?;

        Ok(WebmWriter {
            out,
            segment_body,
            segment_size_pos,
            seek_head_pos,
            info_pos,
            tracks_pos,
            duration_pos,
            width_pos,
            height_pos,
            dims: None,
            cluster: Vec::new(),
            cues: Vec::new(),
            end_ms: 0,
            flushed_ms: 0,
            video_frames: 0,
            audio_frames: 0,
        })
    }

    /// Adds a VP8 frame at `ms` (milliseconds since the recording started).
    pub fn video(&mut self, ms: i64, keyframe: bool, data: &[u8]) -> io::Result<()> {
        if keyframe {
            if self.dims.is_none() {
                self.dims = vp8_dimensions(data);
            }
            let start = self.cluster.iter().map(|b| b.ms).min();
            if start.is_some_and(|s| ms - s >= CLUSTER_MIN_MS) {
                self.flush_cluster()?;
            }
        }
        self.video_frames += 1;
        self.push(VIDEO_TRACK, ms, keyframe, data)
    }

    /// Adds an Opus packet at `ms`.
    pub fn audio(&mut self, ms: i64, data: &[u8]) -> io::Result<()> {
        self.audio_frames += 1;
        self.push(AUDIO_TRACK, ms, true, data)
    }

    fn push(&mut self, track: u64, ms: i64, keyframe: bool, data: &[u8]) -> io::Result<()> {
        // A frame that arrives late (network jitter) is kept, at the end of what is written.
        let ms = ms.max(self.flushed_ms);
        if let Some(start) = self.cluster.iter().map(|b| b.ms).min() {
            if ms - start >= CLUSTER_MAX_MS {
                self.flush_cluster()?;
            }
        }
        self.end_ms = self.end_ms.max(ms);
        self.cluster.push(Block {
            track,
            ms,
            keyframe,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn flush_cluster(&mut self) -> io::Result<()> {
        if self.cluster.is_empty() {
            return Ok(());
        }
        let mut blocks = std::mem::take(&mut self.cluster);
        blocks.sort_by_key(|b| b.ms);
        let start = blocks[0].ms;
        self.flushed_ms = blocks[blocks.len() - 1].ms;
        let mut body = uint(CLUSTER_TIMESTAMP, start as u64);
        for b in &blocks {
            let mut sb = vint(b.track);
            sb.extend(((b.ms - start).min(i16::MAX as i64) as i16).to_be_bytes());
            sb.push(if b.keyframe { 0x80 } else { 0x00 });
            sb.extend(&b.data);
            body.extend(element(SIMPLE_BLOCK, &sb));
        }
        let pos = self.out.stream_position()?;
        self.out.write_all(&element(CLUSTER, &body))?;
        // Seek to keyframes of the video, or anywhere in an audio-only recording.
        let has_video = self.video_frames > 0;
        if let Some(first) = blocks.iter().find(|b| {
            if has_video {
                b.track == VIDEO_TRACK && b.keyframe
            } else {
                b.track == AUDIO_TRACK
            }
        }) {
            self.cues.push((first.ms, first.track, pos - self.segment_body));
        }
        Ok(())
    }

    /// Writes the last cluster, cues and seek head, and patches sizes and duration.
    pub fn finish(mut self) -> io::Result<FinishedWebm> {
        self.flush_cluster()?;
        let cues_pos = self.out.stream_position()?;
        if !self.cues.is_empty() {
            let points: Vec<u8> = self
                .cues
                .iter()
                .flat_map(|(ms, track, pos)| {
                    element(
                        CUE_POINT,
                        &[
                            uint(CUE_TIME, *ms as u64),
                            element(CUE_TRACK_POSITIONS, &[uint(CUE_TRACK, *track), uint(CUE_CLUSTER_POSITION, *pos)].concat()),
                        ]
                        .concat(),
                    )
                })
                .collect();
            self.out.write_all(&element(CUES, &points))?;
        }
        let end = self.out.stream_position()?;
        let rel = |p: u64| p - self.segment_body;
        let mut seeks = vec![(INFO, rel(self.info_pos)), (TRACKS, rel(self.tracks_pos))];
        if !self.cues.is_empty() {
            seeks.push((CUES, rel(cues_pos)));
        }
        let seek_head = element(
            SEEK_HEAD,
            &seeks
                .iter()
                .flat_map(|(id, pos)| element(SEEK, &[element(SEEK_ID, &id_bytes(*id)), uint8(SEEK_POSITION, *pos)].concat()))
                .collect::<Vec<u8>>(),
        );
        let mut reserved = seek_head;
        let pad = SEEK_HEAD_RESERVED - reserved.len();
        reserved.extend(void(pad));
        self.out.seek(SeekFrom::Start(self.seek_head_pos))?;
        self.out.write_all(&reserved)?;
        self.out.seek(SeekFrom::Start(self.segment_size_pos))?;
        self.out.write_all(&vint8(end - self.segment_body))?;
        self.out.seek(SeekFrom::Start(self.duration_pos))?;
        // Last frame plus one typical frame length.
        let duration = if self.end_ms > 0 { self.end_ms + 20 } else { 0 };
        self.out.write_all(&(duration as f64).to_be_bytes())?;
        let (w, h) = self.dims.unwrap_or((640, 360));
        self.out.seek(SeekFrom::Start(self.width_pos))?;
        self.out.write_all(&(w as u64).to_be_bytes())?;
        self.out.seek(SeekFrom::Start(self.height_pos))?;
        self.out.write_all(&(h as u64).to_be_bytes())?;
        self.out.flush()?;
        Ok(FinishedWebm {
            duration_ms: duration as u64,
            bytes: end,
            video_frames: self.video_frames,
            audio_frames: self.audio_frames,
            dimensions: self.dims,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FinishedWebm {
    pub duration_ms: u64,
    pub bytes: u64,
    pub video_frames: u64,
    pub audio_frames: u64,
    pub dimensions: Option<(u32, u32)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vints_are_minimal_and_valid() {
        assert_eq!(vint(0), [0x80]);
        assert_eq!(vint(126), [0xFE]);
        assert_eq!(vint(127), [0x40, 0x7F]);
        assert_eq!(vint(16382), [0x7F, 0xFE]);
        assert_eq!(vint(16383), [0x20, 0x3F, 0xFF]);
        assert_eq!(vint8(5), [0x01, 0, 0, 0, 0, 0, 0, 5]);
        assert_eq!(id_bytes(SEGMENT), [0x18, 0x53, 0x80, 0x67]);
        assert_eq!(id_bytes(SIMPLE_BLOCK), [0xA3]);
        assert_eq!(void(2), [0xEC, 0x80]);
        assert_eq!(void(200).len(), 200);
    }

    #[test]
    fn reads_vp8_keyframe_dimensions() {
        // Frame tag (keyframe bit clear), start code, 1280x720.
        let mut f = vec![0x50, 0x2A, 0x00, 0x9D, 0x01, 0x2A];
        f.extend(1280u16.to_le_bytes());
        f.extend(720u16.to_le_bytes());
        assert_eq!(vp8_dimensions(&f), Some((1280, 720)));
        f[0] |= 1; // interframe
        assert_eq!(vp8_dimensions(&f), None);
    }

    #[test]
    fn writes_a_well_formed_file() {
        let dir = std::env::temp_dir().join(format!("ferrite-webm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.webm");
        let mut w = WebmWriter::create(&path, "test", 2).unwrap();
        let mut key = vec![0x50, 0x2A, 0x00, 0x9D, 0x01, 0x2A];
        key.extend(320u16.to_le_bytes());
        key.extend(180u16.to_le_bytes());
        for i in 0..150i64 {
            w.audio(i * 20, &[0xFC, 0x01]).unwrap();
            if i % 2 == 0 {
                w.video(i * 20, i % 60 == 0, if i % 60 == 0 { &key } else { &[0x51, 0, 0] }).unwrap();
            }
        }
        let done = w.finish().unwrap();
        assert_eq!((done.audio_frames, done.video_frames), (150, 75));
        assert_eq!(done.dimensions, Some((320, 180)));
        assert_eq!(done.duration_ms, 149 * 20 + 20);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, done.bytes);
        assert_eq!(&bytes[..4], &[0x1A, 0x45, 0xDF, 0xA3]);
        // The SeekHead replaced the reserved Void and the segment size was patched.
        let seg = bytes.windows(4).position(|w| w == [0x18, 0x53, 0x80, 0x67]).unwrap();
        assert_ne!(&bytes[seg + 4..seg + 12], &[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&bytes[seg + 12..seg + 16], &[0x11, 0x4D, 0x9B, 0x74]);
        // Three keyframes → three cue points (CuePoint, size, CueTime).
        assert_eq!(bytes.windows(3).filter(|w| w[0] == 0xBB && w[2] == 0xB3).count(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Muxes real VP8 and Opus frames (encoded by ffmpeg) and checks that ffmpeg decodes
    /// every frame, reads the right duration and size, and can seek. Skipped when ffmpeg is
    /// not installed.
    #[test]
    fn real_frames_decode_and_seek_in_ffmpeg() {
        use std::process::Command;
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("ffmpeg not installed: skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("ferrite-webm-ff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ff = |args: &[&str]| {
            let out = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).output().unwrap();
            assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let (ivf, ogg, webm) = (dir.join("v.ivf"), dir.join("a.ogg"), dir.join("out.webm"));
        ff(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30",
            "-t",
            "3",
            "-c:v",
            "libvpx",
            "-g",
            "30",
            "-b:v",
            "300k",
            ivf.to_str().unwrap(),
        ]);
        ff(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "3",
            "-c:a",
            "libopus",
            "-frame_duration",
            "20",
            ogg.to_str().unwrap(),
        ]);

        // IVF: 32-byte file header, then [size u32 LE][pts u64 LE][frame] (time base 1/30).
        let v = std::fs::read(&ivf).unwrap();
        let mut frames = Vec::new();
        let mut i = 32;
        while i + 12 <= v.len() {
            let size = u32::from_le_bytes(v[i..i + 4].try_into().unwrap()) as usize;
            let pts = u64::from_le_bytes(v[i + 4..i + 12].try_into().unwrap());
            frames.push((pts as i64 * 1000 / 30, v[i + 12..i + 12 + size].to_vec()));
            i += 12 + size;
        }
        // Ogg: pages of lacing-delimited packets; the first two packets are headers.
        let a = std::fs::read(&ogg).unwrap();
        let mut packets = Vec::new();
        let (mut i, mut cur) = (0, Vec::new());
        while i + 27 <= a.len() && &a[i..i + 4] == b"OggS" {
            let segs = a[i + 26] as usize;
            let table = &a[i + 27..i + 27 + segs];
            let mut p = i + 27 + segs;
            for &len in table {
                cur.extend_from_slice(&a[p..p + len as usize]);
                p += len as usize;
                if len < 255 {
                    packets.push(std::mem::take(&mut cur));
                }
            }
            i = p;
        }
        let audio: Vec<Vec<u8>> = packets.into_iter().skip(2).collect();

        let mut w = WebmWriter::create(&webm, "ffmpeg check", 1).unwrap();
        let (mut vi, mut ai) = (0, 0);
        while vi < frames.len() || ai < audio.len() {
            let a_ms = ai as i64 * 20;
            if vi < frames.len() && (ai >= audio.len() || frames[vi].0 <= a_ms) {
                let key = frames[vi].1[0] & 1 == 0;
                w.video(frames[vi].0, key, &frames[vi].1).unwrap();
                vi += 1;
            } else {
                w.audio(a_ms, &audio[ai]).unwrap();
                ai += 1;
            }
        }
        let done = w.finish().unwrap();
        assert_eq!(done.dimensions, Some((320, 240)));

        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_frames",
                "-show_entries",
                "stream=codec_name,nb_read_frames,width,height:format=duration",
                "-of",
                "json",
            ])
            .arg(&webm)
            .output()
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        let streams = info["streams"].as_array().unwrap();
        let by = |c: &str| streams.iter().find(|s| s["codec_name"] == c).unwrap().clone();
        assert_eq!(by("vp8")["nb_read_frames"], frames.len().to_string());
        assert_eq!(by("vp8")["width"], 320);
        assert_eq!(by("opus")["nb_read_frames"], audio.len().to_string());
        let duration: f64 = info["format"]["duration"].as_str().unwrap().parse().unwrap();
        assert!((duration - 3.0).abs() < 0.1, "duration {duration}");
        // Decodes cleanly from the start and after a seek (uses the cues).
        ff(&["-i", webm.to_str().unwrap(), "-f", "null", "-"]);
        ff(&["-ss", "2", "-i", webm.to_str().unwrap(), "-frames:v", "5", "-f", "null", "-"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
