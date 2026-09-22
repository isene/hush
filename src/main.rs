//! hush — a voice call that sends nothing while you are quiet.
//!
//! Two people join a room by name. Each sends twenty milliseconds of
//! Opus whenever they are actually speaking and not one byte when they
//! are not. On an ordinary conversation each end talks less than half
//! the time, and says nothing at all for the pauses inside its own
//! sentences, so the line is idle for most of the call.
//!
//! The screen shows that as it happens: what each end is doing, how
//! much has gone each way, and the share of the call that was worth
//! sending.
//!
//! Neither end can reach the other through a home router, so both send
//! to a relay with a real address. Run one with `hush --relay`.

mod audio;

use hush::net;

use audio::{Gate, FRAME, RATE};
use audiopus::coder::{Decoder, Encoder};
use audiopus::{Application, Bitrate, Channels, SampleRate};
use crust::style;
use crust::{seq, Crust, Cursor, Input, Pane};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RUST_RGB: (u8, u8, u8) = (247, 76, 0);
const HEAD_RGB: (u8, u8, u8) = (247, 140, 60);
const LIVE_RGB: (u8, u8, u8) = (120, 230, 140);
const IDLE_RGB: (u8, u8, u8) = (110, 110, 125);
const BAR_BG: (u8, u8, u8) = (38, 38, 38);
const DIM_RGB: (u8, u8, u8) = (140, 140, 150);

/// What the call knows about itself, for the screen.
struct State {
    /// Everyone the relay says is in the room.
    here: Vec<String>,
    /// How loud each far end was in the last frame it sent.
    heard: BTreeMap<String, f32>,
    /// My own level, and whether the gate is open.
    level: f32,
    floor: f32,
    open: bool,
    /// Set when a key asks for the microphone to be held shut.
    muted: bool,
    last_in: Option<Instant>,
}

/// Counters that two threads touch, so they are kept apart from the rest.
struct Tally {
    sent_bytes: AtomicU64,
    sent_frames: AtomicU64,
    recv_bytes: AtomicU64,
    recv_frames: AtomicU64,
    /// Frames the microphone gave us, spoken or not.
    heard_frames: AtomicU64,
    /// Frames the far end sent that never arrived.
    lost_frames: AtomicU64,
}

fn main() {
    let mut relay_port = 0u16;
    // Whose relay to go through. Nobody else's address belongs in this
    // file, so it comes from ~/.hush or from the command line.
    let mut server = config_server();
    let mut room = String::new();
    let mut name = std::env::var("USER").unwrap_or_else(|_| "someone".into());
    let mut mic = String::from("default");
    let mut out = String::from("default");
    // The echo canceller is on unless a device is named by hand, since
    // naming one says which microphone to use, and the canceller's is
    // a different one.
    let mut want_aec = true;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--relay" => {
                relay_port = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(7777);
                i += 1;
            }
            "-s" | "--server" => {
                server = args.get(i + 1).cloned().unwrap_or(server);
                i += 1;
            }
            "-n" | "--name" => {
                name = args.get(i + 1).cloned().unwrap_or(name);
                i += 1;
            }
            "--mic" => {
                mic = args.get(i + 1).cloned().unwrap_or(mic);
                want_aec = false;
                i += 1;
            }
            "--out" => {
                out = args.get(i + 1).cloned().unwrap_or(out);
                want_aec = false;
                i += 1;
            }
            "--no-aec" => want_aec = false,
            "-v" | "--version" => {
                println!("hush {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                help();
                return;
            }
            other => room = other.to_string(),
        }
        i += 1;
    }

    if relay_port != 0 {
        if let Err(e) = net::relay(relay_port) {
            eprintln!("hush: relay: {e}");
            std::process::exit(1);
        }
        return;
    }

    if room.is_empty() {
        help();
        std::process::exit(1);
    }
    if server.is_empty() {
        eprintln!("hush: no relay to go through.");
        eprintln!("Put one line in ~/.hush, `host:port`, or pass -s host:port.");
        eprintln!("Run `hush --relay` on a machine both ends can reach.");
        std::process::exit(1);
    }

    // Loaded here and dropped when the call ends, so nothing of it runs
    // between calls.
    let aec = if want_aec { audio::Aec::start() } else { None };
    if aec.is_some() {
        mic = audio::AEC_MIC.to_string();
        out = audio::AEC_OUT.to_string();
    }

    if let Err(e) = call(&server, &room, &name, &mic, &out, aec.is_some()) {
        eprintln!("hush: {e}");
        std::process::exit(1);
    }
}

/// The relay named in `~/.hush`, one line of `host:port`, or nothing.
fn config_server() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    std::fs::read_to_string(std::path::Path::new(&home).join(".hush"))
        .map(|t| {
            t.lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_default()
}

fn help() {
    println!("hush — a voice call that sends nothing while you are quiet");
    println!();
    println!("Usage: hush <room> [-n NAME] [-s HOST:PORT]");
    println!("       hush --relay [PORT]");
    println!();
    println!("  <room>        the room to join; anyone using the same word is on the call");
    println!("  -n NAME       what the others see you as (default: your login name)");
    println!("  -s HOST:PORT  the relay to go through (default: the line in ~/.hush)");
    println!("  --mic DEVICE  an ALSA device other than the default");
    println!("  --out DEVICE  likewise for the speaker");
    println!("  --no-aec      leave the echo canceller out");
    println!("  --relay PORT  be the relay instead of joining a call");
    println!();
    println!("PipeWire's echo canceller is used when it is there, so the far end");
    println!("does not hear itself. Headphones are still the safer answer.");
}

fn call(server: &str, room: &str, name: &str, mic: &str, out: &str, aec: bool) -> std::io::Result<()> {
    let addr = server
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other(format!("cannot find {server}")))?;
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect(addr)?;
    sock.send(&net::join(room, name))?;

    let state = Arc::new(Mutex::new(State {
        here: vec![name.to_string()],
        heard: BTreeMap::new(),
        level: 0.0,
        floor: 0.0,
        open: false,
        muted: false,
        last_in: None,
    }));
    let tally = Arc::new(Tally {
        sent_bytes: AtomicU64::new(0),
        sent_frames: AtomicU64::new(0),
        recv_bytes: AtomicU64::new(0),
        recv_frames: AtomicU64::new(0),
        heard_frames: AtomicU64::new(0),
        lost_frames: AtomicU64::new(0),
    });
    let going = Arc::new(AtomicBool::new(true));

    // The sound coming back, one queue of frames waiting to be played.
    let waiting: Arc<Mutex<Vec<Vec<i16>>>> = Arc::new(Mutex::new(Vec::new()));

    let talk = std::thread::spawn({
        let (sock, state, tally, going) = (sock.try_clone()?, state.clone(), tally.clone(), going.clone());
        let (room, name, mic) = (room.to_string(), name.to_string(), mic.to_string());
        move || speak(sock, &room, &name, &mic, aec, state, tally, going)
    });
    let listen = std::thread::spawn({
        let (sock, state, tally, going, waiting) = (sock.try_clone()?, state.clone(), tally.clone(), going.clone(), waiting.clone());
        move || hear(sock, state, tally, going, waiting)
    });
    let play = std::thread::spawn({
        let (going, waiting, out) = (going.clone(), waiting.clone(), out.to_string());
        move || play_back(&out, aec, waiting, going)
    });
    let beat = std::thread::spawn({
        let (sock, going) = (sock.try_clone()?, going.clone());
        let (room, name) = (room.to_string(), name.to_string());
        move || {
            // A packet a second holds the way open through both routers,
            // which is the only thing that has to happen during silence.
            while going.load(Ordering::Relaxed) {
                let _ = sock.send(&net::ping(&room, &name));
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });

    screen(room, name, server, aec, &state, &tally, &going);

    going.store(false, Ordering::Relaxed);
    let _ = sock.send(&net::leave(room, name));
    let _ = talk.join();
    let _ = listen.join();
    let _ = play.join();
    let _ = beat.join();
    Ok(())
}

/// Read the microphone, decide whether it is worth sending, and send it.
fn speak(
    sock: UdpSocket,
    room: &str,
    name: &str,
    mic: &str,
    aec: bool,
    state: Arc<Mutex<State>>,
    tally: Arc<Tally>,
    going: Arc<AtomicBool>,
) {
    let mut child = match audio::microphone(mic, aec) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut src = match child.stdout.take() {
        Some(s) => s,
        None => return,
    };
    let enc = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip);
    let mut enc = match enc {
        Ok(e) => e,
        Err(_) => return,
    };
    // Twenty kilobits is plenty for a voice and little for a line.
    let _ = enc.set_bitrate(Bitrate::BitsPerSecond(20_000));
    let mut raw = vec![0u8; FRAME * 2];
    let mut gate = Gate::new();
    let mut packet = vec![0u8; net::PACKET_MAX];
    let mut seq = 0u32;

    while going.load(Ordering::Relaxed) {
        if src.read_exact(&mut raw).is_err() {
            break;
        }
        let frame = audio::to_samples(&raw);
        let level = audio::loudness(&frame);
        let open = gate.speaking(level);
        let muted = {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.level = level;
            s.floor = gate.floor();
            s.open = open && !s.muted;
            s.muted
        };
        tally.heard_frames.fetch_add(1, Ordering::Relaxed);
        if !open || muted {
            // The whole point: nothing goes out at all.
            continue;
        }
        let n = match enc.encode(&frame, &mut packet) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let out = net::audio(room, name, seq, &packet[..n]);
        seq = seq.wrapping_add(1);
        if sock.send(&out).is_ok() {
            tally.sent_bytes.fetch_add(out.len() as u64, Ordering::Relaxed);
            tally.sent_frames.fetch_add(1, Ordering::Relaxed);
        }
    }
    let _ = child.kill();
}

/// Take packets off the wire and turn them back into sound.
fn hear(
    sock: UdpSocket,
    state: Arc<Mutex<State>>,
    tally: Arc<Tally>,
    going: Arc<AtomicBool>,
    waiting: Arc<Mutex<Vec<Vec<i16>>>>,
) {
    let mut dec = match Decoder::new(SampleRate::Hz48000, Channels::Mono) {
        Ok(d) => d,
        Err(_) => return,
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(400)));
    let mut buf = vec![0u8; net::PACKET_MAX];
    // The last frame number each speaker sent, so a gap can be counted.
    let mut last: BTreeMap<String, u32> = BTreeMap::new();
    while going.load(Ordering::Relaxed) {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        match net::parse(&buf[..n]) {
            net::In::Speech { from, seq, opus } => {
                // A jump in the count is a packet that never arrived.
                // A step back is one that arrived late, which the queue
                // will play in the wrong place rather than not at all.
                if let Some(prev) = last.insert(from.clone(), seq) {
                    let gap = seq.wrapping_sub(prev);
                    if gap > 1 && gap < 100 {
                        tally.lost_frames.fetch_add((gap - 1) as u64, Ordering::Relaxed);
                    }
                }
                let mut frame = vec![0i16; FRAME];
                if dec.decode(Some(&opus[..]), &mut frame[..], false).is_err() {
                    continue;
                }
                tally.recv_bytes.fetch_add(n as u64, Ordering::Relaxed);
                tally.recv_frames.fetch_add(1, Ordering::Relaxed);
                {
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    s.heard.insert(from, audio::loudness(&frame));
                    s.last_in = Some(Instant::now());
                }
                let mut q = waiting.lock().unwrap_or_else(|e| e.into_inner());
                // A queue that grows is a queue nobody is listening to.
                if q.len() < 12 {
                    q.push(frame);
                }
            }
            net::In::Who(names) => {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.here = names;
            }
            net::In::Other => {}
        }
    }
}

/// Feed the speaker, twenty milliseconds at a time, silence when there
/// is nothing waiting.
fn play_back(out: &str, aec: bool, waiting: Arc<Mutex<Vec<Vec<i16>>>>, going: Arc<AtomicBool>) {
    let mut child = match audio::speaker(out, aec) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut sink = match child.stdin.take() {
        Some(s) => s,
        None => return,
    };
    let quiet = audio::to_bytes(&vec![0i16; FRAME]);
    let step = Duration::from_micros(1_000_000 * FRAME as u64 / RATE as u64);
    let mut next = Instant::now();
    while going.load(Ordering::Relaxed) {
        next += step;
        let frame = {
            let mut q = waiting.lock().unwrap_or_else(|e| e.into_inner());
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        };
        let bytes = match &frame {
            Some(f) => audio::to_bytes(f),
            None => quiet.clone(),
        };
        if sink.write_all(&bytes).is_err() {
            break;
        }
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    let _ = child.kill();
}

fn move_to(row: u16, col: u16) -> String {
    Cursor::at(col, row)
}

/// A bar that shows a level, with the gate's own threshold marked on it.
fn meter(level: f32, floor: f32, open: bool, width: usize) -> String {
    let scale = |v: f32| ((v.max(1e-5).log10() + 3.0) / 3.0).clamp(0.0, 1.0);
    let lit = (scale(level) * width as f32) as usize;
    let mark = (scale(floor * 2.6) * width as f32) as usize;
    let rgb = if open { LIVE_RGB } else { IDLE_RGB };
    let mut s = String::new();
    for i in 0..width {
        if i == mark {
            s.push_str(&style::rgb("│", Some((200, 180, 90)), None, ""));
        } else if i < lit {
            s.push_str(&style::rgb("█", Some(rgb), None, ""));
        } else {
            s.push_str(&style::rgb("─", Some((55, 55, 68)), None, ""));
        }
    }
    s
}

fn rate(bytes: u64, secs: f64) -> String {
    if secs <= 0.0 {
        return "0 B/s".into();
    }
    let v = bytes as f64 / secs;
    if v < 1024.0 {
        format!("{v:.0} B/s")
    } else {
        format!("{:.1} kB/s", v / 1024.0)
    }
}

fn total(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} kB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// The screen, and the keys. Redrawn five times a second, which is
/// enough for a level meter and cheap enough to leave running.
fn screen(
    room: &str,
    name: &str,
    server: &str,
    aec: bool,
    state: &Arc<Mutex<State>>,
    tally: &Arc<Tally>,
    going: &Arc<AtomicBool>,
) {
    Crust::init();
    Crust::set_app_identity("Hush");
    let (cols, rows) = Crust::terminal_size();
    let mut status = Pane::new(1, rows, cols, 1, 250, 236);
    status.scroll = false;
    let began = Instant::now();

    while going.load(Ordering::Relaxed) {
        {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            let secs = began.elapsed().as_secs_f64();
            let sent = tally.sent_bytes.load(Ordering::Relaxed);
            let recv = tally.recv_bytes.load(Ordering::Relaxed);
            let frames = tally.heard_frames.load(Ordering::Relaxed).max(1);
            let spoke = tally.sent_frames.load(Ordering::Relaxed);
            let share = spoke as f64 / frames as f64 * 100.0;
            let lost = tally.lost_frames.load(Ordering::Relaxed);
            let got = tally.recv_frames.load(Ordering::Relaxed);

            let head = format!(
                " {}  {}  {}",
                style::rgb("hush", Some(RUST_RGB), None, "b"),
                style::bold(room),
                style::dim(&format!("through {server}"))
            );
            let right = format!("{}  {} ", name, fmt_time(secs));
            let pad = (cols as usize)
                .saturating_sub(crust::display_width(&head))
                .saturating_sub(crust::display_width(&right));
            let armed = style::rgb("", None, Some(BAR_BG), "");
            let armed = armed.trim_end_matches(style::RESET);
            let line = head.replace(style::RESET, &format!("{}{}", style::RESET, armed));
            print!(
                "{}{}",
                move_to(1, 1),
                style::rgb(&format!("{line}{}{right}", " ".repeat(pad)), None, Some(BAR_BG), "")
            );

            let w = (cols as usize).saturating_sub(34).clamp(10, 60);
            let mut row = 3u16;
            let mine = if s.muted {
                style::rgb("muted", Some((230, 120, 110)), None, "b")
            } else if s.open {
                style::rgb("sending", Some(LIVE_RGB), None, "b")
            } else {
                style::rgb("quiet", Some(IDLE_RGB), None, "")
            };
            print!(
                "{}  {:<14} {} {}{}",
                move_to(row, 1),
                style::bold("you"),
                meter(s.level, s.floor, s.open && !s.muted, w),
                mine,
                seq::ERASE_EOL
            );
            row += 2;

            let others: Vec<&String> = s.here.iter().filter(|n| *n != name).collect();
            if others.is_empty() {
                print!(
                    "{}  {}{}",
                    move_to(row, 1),
                    style::rgb("waiting for someone else to join this room", Some(DIM_RGB), None, "i"),
                    seq::ERASE_EOL
                );
                row += 1;
            }
            for other in others {
                let level = *s.heard.get(other).unwrap_or(&0.0);
                let live = s.last_in.map(|t| t.elapsed() < Duration::from_millis(250)).unwrap_or(false);
                let says = if live {
                    style::rgb("speaking", Some(LIVE_RGB), None, "b")
                } else {
                    style::rgb("quiet", Some(IDLE_RGB), None, "")
                };
                print!(
                    "{}  {:<14} {} {}{}",
                    move_to(row, 1),
                    style::bold(other),
                    meter(if live { level } else { 0.0 }, 0.0, live, w),
                    says,
                    seq::ERASE_EOL
                );
                row += 1;
            }

            row += 1;
            print!(
                "{}  {}{}",
                move_to(row, 1),
                style::rgb(
                    &format!(
                        "sent {}  ({})      received {}  ({}){}",
                        total(sent),
                        rate(sent, secs),
                        total(recv),
                        rate(recv, secs),
                        if lost == 0 {
                            String::new()
                        } else {
                            format!("      {lost} frames lost of {}", got + lost)
                        }
                    ),
                    Some(DIM_RGB),
                    None,
                    ""
                ),
                seq::ERASE_EOL
            );
            row += 1;
            print!(
                "{}  {}{}",
                move_to(row, 1),
                style::rgb(
                    &format!("the line carried your voice {share:.0}% of the time"),
                    Some(HEAD_RGB),
                    None,
                    ""
                ),
                seq::ERASE_EOL
            );
        }
        status.say(&format!(
            " {}  {}  {}  {}",
            style::rgb("m", Some(HEAD_RGB), None, "b"),
            style::dim("mute · q hangs up"),
            style::dim(if aec { "echo cancelled" } else { "no echo cancelling · wear headphones" }),
            style::dim(&format!("v{}", env!("CARGO_PKG_VERSION")))
        ));
        std::io::stdout().flush().ok();

        if let Some(key) = Input::getchr(Some(200)) {
            match key.as_str() {
                "q" | "ESC" => break,
                "m" => {
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    s.muted = !s.muted;
                }
                _ => {}
            }
        }
    }
    going.store(false, Ordering::Relaxed);
    Crust::cleanup();
}

fn fmt_time(secs: f64) -> String {
    let s = secs as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_and_rates_read_the_way_a_person_says_them() {
        assert_eq!(total(900), "900 B");
        assert_eq!(total(2048), "2.0 kB");
        assert_eq!(total(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(rate(0, 0.0), "0 B/s");
        assert_eq!(rate(1000, 1.0), "1000 B/s");
        assert_eq!(rate(4096, 2.0), "2.0 kB/s");
    }

    #[test]
    fn the_meter_is_as_wide_as_it_is_asked_to_be() {
        let bar = meter(0.05, 0.002, true, 20);
        let blocks = bar.matches('█').count() + bar.matches('─').count() + bar.matches('│').count();
        assert_eq!(blocks, 20);
    }

    #[test]
    fn a_louder_voice_lights_more_of_the_meter() {
        let soft = meter(0.002, 0.001, true, 30).matches('█').count();
        let loud = meter(0.2, 0.001, true, 30).matches('█').count();
        assert!(loud > soft, "a shout lights more of the bar than a whisper");
    }

    #[test]
    fn the_clock_counts_in_minutes_and_seconds() {
        assert_eq!(fmt_time(0.0), "00:00");
        assert_eq!(fmt_time(65.0), "01:05");
        assert_eq!(fmt_time(3600.0), "60:00");
    }
}
