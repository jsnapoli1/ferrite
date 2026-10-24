//! Load test of the SFU without browsers: N participants in one call, each a str0m client in
//! this process with its own UDP socket, sending one video track (synthetic VP8 frames at the
//! given rate and bitrate) and one audio track (Opus-sized packets every 20 ms), and receiving
//! everyone else. Reports what the SFU thread spent (its own CPU time from /proc) against what
//! it forwarded, and the frame rate each receiver got.
//!
//!   cargo run --release -p ferrite-sfu --example sfu-load -- [PEOPLE] [FPS] [KBPS] [SECONDS]
//!   (defaults: 4 people, 30 fps, 1500 kbit/s, 10 s)
//!
//! The payload is not real VP8, so this measures forwarding (SRTP, packetizing, pacing,
//! RTCP, the event loop), not decoding. Linux only (it reads /proc/self/task).

use serde_json::json;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::channel::ChannelId;
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

struct Stats {
    video_frames: AtomicU64,
    audio_packets: AtomicU64,
}

fn arg(i: usize, default: u64) -> u64 {
    std::env::args().nth(i).and_then(|a| a.parse().ok()).unwrap_or(default)
}

/// CPU seconds (user + system) of this process's thread named `name`.
fn thread_cpu(name: &str) -> f64 {
    let ticks = 100.0; // CLK_TCK on Linux
    for entry in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
        let p = entry.path();
        if std::fs::read_to_string(p.join("comm")).is_ok_and(|c| c.trim() == name) {
            let stat = std::fs::read_to_string(p.join("stat")).unwrap_or_default();
            // Fields after the parenthesized name: state is field 3, utime 14, stime 15.
            let rest = stat.rsplit_once(')').map(|x| x.1).unwrap_or("");
            let f: Vec<&str> = rest.split_whitespace().collect();
            let ut: f64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let st: f64 = f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            return (ut + st) / ticks;
        }
    }
    0.0
}

fn main() {
    let people = arg(1, 4) as usize;
    let fps = arg(2, 30);
    let kbps = arg(3, 1500);
    let seconds = arg(4, 10);
    let sfu = Arc::new(
        ferrite_sfu::Sfu::start(ferrite_sfu::SfuConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertise: vec![],
        })
        .expect("start the SFU"),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let stats: Vec<Arc<Stats>> = (0..people)
        .map(|_| {
            Arc::new(Stats {
                video_frames: AtomicU64::new(0),
                audio_packets: AtomicU64::new(0),
            })
        })
        .collect();
    let mut threads = Vec::new();
    for (i, st) in stats.iter().enumerate() {
        let (sfu, stop, st) = (sfu.clone(), stop.clone(), st.clone());
        threads.push(std::thread::spawn(move || participant(i, &sfu, &stop, &st, fps, kbps)));
        std::thread::sleep(Duration::from_millis(150));
    }
    // Let everyone connect and the renegotiations settle, then measure.
    std::thread::sleep(Duration::from_secs(4));
    let (cpu0, s0, f0) = (thread_cpu("ferrite-sfu"), sfu.stats(), frames(&stats));
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(seconds));
    let (cpu1, s1, f1) = (thread_cpu("ferrite-sfu"), sfu.stats(), frames(&stats));
    let w = t0.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    let cpu = cpu1 - cpu0;
    let mbit = (s1.bytes_forwarded - s0.bytes_forwarded) as f64 * 8.0 / w / 1e6;
    let expected_streams = (people * (people - 1)) as f64;
    let report = json!({
        "people": people, "fps": fps, "kbps": kbps, "seconds": w,
        "clients": s1.clients,
        "sfu_cpu_core_share": (cpu / w * 1000.0).round() / 1000.0,
        "forwarded_mbit_per_s": (mbit * 10.0).round() / 10.0,
        "forwarded_writes_per_s": ((s1.packets_forwarded - s0.packets_forwarded) as f64 / w).round(),
        "forwarded_mbit_per_s_per_core": if cpu > 0.0 { (mbit / (cpu / w)).round() } else { 0.0 },
        "received_video_fps_per_stream": ((f1.0 - f0.0) as f64 / w / expected_streams * 10.0).round() / 10.0,
        "received_audio_pps_per_stream": ((f1.1 - f0.1) as f64 / w / expected_streams).round(),
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn frames(stats: &[Arc<Stats>]) -> (u64, u64) {
    stats.iter().fold((0, 0), |(v, a), s| {
        (v + s.video_frames.load(Ordering::Relaxed), a + s.audio_packets.load(Ordering::Relaxed))
    })
}

fn participant(i: usize, sfu: &ferrite_sfu::Sfu, stop: &AtomicBool, st: &Stats, fps: u64, kbps: u64) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let local = socket.local_addr().unwrap();
    let mut rtc = Rtc::builder().clear_codecs().enable_vp8(true).enable_opus(true, false).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut api = rtc.sdp_api();
    let video: Mid = api.add_media(MediaKind::Video, Direction::SendOnly, Some(format!("p{i}")), None, None);
    let audio: Mid = api.add_media(MediaKind::Audio, Direction::SendOnly, Some(format!("p{i}")), None, None);
    let _ = api.add_channel("ferrite".into());
    let (offer, pending) = api.apply().unwrap();
    let answer = sfu
        .connect("load", &format!("p{i}"), serde_json::to_value(&offer).unwrap())
        .expect("SFU answer");
    let answer: SdpAnswer = serde_json::from_value(answer).unwrap();
    rtc.sdp_api().accept_answer(pending, answer).unwrap();

    let frame_bytes = (kbps * 1000 / 8 / fps.max(1)) as usize;
    let frame_every = Duration::from_micros(1_000_000 / fps.max(1));
    let audio_every = Duration::from_millis(20);
    let (mut next_frame, mut next_audio) = (Instant::now(), Instant::now());
    let (mut frame_no, mut audio_no) = (0u64, 0u64);
    let mut channel: Option<ChannelId> = None;
    let mut buf = vec![0u8; 2000];
    let _ = socket.set_nonblocking(false);
    while !stop.load(Ordering::Relaxed) && rtc.is_alive() {
        let now = Instant::now();
        if rtc.is_connected() {
            if now >= next_frame {
                if let Some(w) = rtc.writer(video) {
                    let pt = w.payload_params().find(|p| p.spec().codec == Codec::Vp8).map(|p| p.pt());
                    if let Some(pt) = pt {
                        // Byte 0 is the VP8 frame tag: bit 0 clear marks a keyframe (one a second).
                        let mut data = vec![0x5au8; frame_bytes.max(16)];
                        data[0] = if frame_no % fps.max(1) == 0 { 0x10 } else { 0x11 };
                        let ts = MediaTime::from_90khz(frame_no * 90_000 / fps.max(1));
                        let _ = w.write(pt, now, ts, data);
                    }
                }
                frame_no += 1;
                next_frame += frame_every;
            }
            if now >= next_audio {
                if let Some(w) = rtc.writer(audio) {
                    let pt = w.payload_params().find(|p| p.spec().codec == Codec::Opus).map(|p| p.pt());
                    if let Some(pt) = pt {
                        let ts = MediaTime::new(audio_no * 960, str0m::media::Frequency::FORTY_EIGHT_KHZ);
                        let _ = w.write(pt, now, ts, vec![0x78u8; 80]);
                    }
                }
                audio_no += 1;
                next_audio += audio_every;
            }
        }
        // Drive str0m until it has nothing more to do right now.
        let timeout = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = socket.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(e)) => match e {
                    Event::ChannelOpen(id, _) => channel = Some(id),
                    Event::ChannelData(d) => {
                        // Renegotiation offers from the SFU (new participants' tracks).
                        if let Ok(offer) = serde_json::from_slice::<SdpOffer>(&d.data) {
                            if let Ok(answer) = rtc.sdp_api().accept_offer(offer) {
                                if let (Some(mut ch), Ok(j)) = (channel.and_then(|c| rtc.channel(c)), serde_json::to_string(&answer)) {
                                    let _ = ch.write(false, j.as_bytes());
                                }
                            }
                        }
                    }
                    Event::MediaData(m) => {
                        let counter = if m.params.spec().codec == Codec::Vp8 {
                            &st.video_frames
                        } else {
                            &st.audio_packets
                        };
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                },
                Err(_) => return,
            }
        };
        let wake = timeout.min(next_frame).min(next_audio);
        let wait = wake.saturating_duration_since(Instant::now()).max(Duration::from_micros(200));
        let _ = socket.set_read_timeout(Some(wait));
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                let destination: SocketAddr = local;
                if let Ok(contents) = buf[..n].try_into() {
                    let _ = rtc.handle_input(Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination,
                            contents,
                        },
                    ));
                }
            }
            Err(_) => {
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
        }
    }
}
