//! The picture, when it is wanted.
//!
//! The camera is `ffmpeg`, as the microphone is `arecord`: one process
//! for the length of the call, streaming through a pipe. It encodes on
//! the graphics chip where there is one, so the processor stays cold.
//!
//! The same idea as the sound gate runs here. Frames that look like the
//! one before are thrown away before the encoder ever sees them, so a
//! person sitting still sends nothing at all. Measured on a still scene
//! at 480x360: one frame, 259 bytes, for four seconds.
//!
//! A picture does not fit in a packet, so each one goes out in pieces
//! and is put back together at the far end. A piece that never arrives
//! costs that one picture, and the next whole one replaces it.
//!
//! The screen goes the same way as the camera: the same encoder, the
//! same gate, so a screen nobody touches sends nothing either.

use std::io::{BufRead, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// What the camera sends, and how often.
pub const WIDTH: u16 = 480;
pub const HEIGHT: u16 = 360;
pub const FPS: u16 = 15;

/// The graphics chip that does the encoding, where there is one.
const RENDER: &str = "/dev/dri/renderD128";

/// How often the screen is looked at. Text needs no more, and every
/// look that shows a change is a picture to encode and send.
pub const SCREEN_FPS: u16 = 5;

/// The camera, as a stream of encoded pictures. `source` is a device
/// path, the word `test` for a moving picture that needs no camera, or
/// the word `screen` for the screen itself.
pub fn camera(source: &str) -> std::io::Result<Child> {
    let hardware = std::path::Path::new(RENDER).exists();
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    Command::new("ffmpeg")
        .args(encoder_args(source, hardware, &display))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// What ffmpeg is told, to turn a camera or the screen into H.264.
/// `hardware` says whether the graphics chip does the encoding.
fn encoder_args(source: &str, hardware: bool, display: &str) -> Vec<String> {
    let screen = source == "screen";
    let mut args: Vec<String> = Vec::new();
    let mut add = |words: &[&str]| args.extend(words.iter().map(|w| w.to_string()));
    add(&["-hide_banner", "-loglevel", "error"]);
    if hardware {
        add(&["-vaapi_device", RENDER]);
    }
    if screen {
        add(&["-f", "x11grab", "-framerate", &SCREEN_FPS.to_string(), "-i", display]);
    } else if source == "test" {
        // A camera paces itself. A made-up picture does not, so it has to
        // be told, or it floods the line as fast as the chip can encode.
        add(&["-re", "-f", "lavfi", "-i", &format!("testsrc=size={WIDTH}x{HEIGHT}:rate={FPS}")]);
    } else {
        add(&["-f", "v4l2", "-framerate", &FPS.to_string(), "-i", source]);
    }
    // Drop what has not changed, scale to what we send, then hand the
    // frame to the chip. The numbers are how different a frame has to
    // be to count as a new one.
    let gate = if screen {
        // A screen has no noise in it, so any change at all is a real
        // one: a typed letter, the pointer. It is looked for before the
        // scaling, where a letter is still sharp. The picture keeps its
        // shape and fills what it can of 1280 by 720: big enough to
        // read text by, and no more than every decoder takes.
        "mpdecimate=hi=128:lo=128:frac=1,\
         scale=w=1280:h=720:force_original_aspect_ratio=decrease:force_divisible_by=2"
            .to_string()
    } else {
        // These let a face move and keep a still room shut.
        format!("scale={WIDTH}:{HEIGHT},mpdecimate=hi=768:lo=320:frac=0.2")
    };
    add(&["-vf", &format!("{gate}{}", if hardware { ",format=nv12,hwupload" } else { "" })]);
    // The plainest H.264 there is. Browsers decode this one everywhere,
    // and the terminal end is happy with it either way. libx264 calls
    // it `baseline` and knows no other name for it.
    if hardware {
        // The chip gives a picture back when it gets the next one,
        // unless told to work on one at a time.
        add(&["-c:v", "h264_vaapi", "-async_depth", "1", "-profile:v", "constrained_baseline"]);
    } else {
        add(&["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-profile:v", "baseline"]);
    }
    add(&["-level", "31"]);
    // Only the pictures that got through the gate are encoded. Left to
    // itself ffmpeg fills every hole the gate made with an empty picture.
    add(&["-fps_mode", "passthrough"]);
    // No B-frames, so a picture is ready the moment it is encoded.
    //
    // A keyframe when four seconds have gone since the last one. It
    // costs nothing while the picture stands still, because then no
    // frame reaches the encoder at all.
    add(&["-force_key_frames", "expr:if(isnan(prev_forced_t),gte(t,4),gte(t,prev_forced_t+4))"]);
    add(&["-b:v", if screen { "600k" } else { "400k" }, "-g", "600", "-bf", "0"]);
    // Each picture is written out the moment it is done.
    add(&["-flush_packets", "1", "-f", "h264", "-"]);
    args
}

/// The far end's pictures, decoded to plain pixels at the size they
/// were sent. Each comes out with its size written in front of it, so a
/// camera and a screen need nothing told about them beforehand, and the
/// terminal does the one scaling there is.
pub fn decoder() -> std::io::Result<Child> {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-flags", "low_delay"])
        // Left to itself ffmpeg reads twenty pictures before it shows
        // the first, to learn what it has been given. The first one
        // says all of it, and a still screen sends no second.
        .args(["-probesize", "2048", "-fpsprobesize", "0"])
        .args(["-f", "h264", "-i", "-"])
        .args(["-c:v", "ppm", "-f", "image2pipe", "-flush_packets", "1", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// One picture off the decoder: how wide, how high, and its pixels,
/// three bytes each. Nothing once the decoder has stopped.
///
/// The decoder writes each picture as a PPM file: `P6`, the width and
/// height, the brightest value, each followed by white space, then the
/// pixels.
pub fn read_picture(src: &mut impl BufRead) -> Option<(usize, usize, Vec<u8>)> {
    let word = |src: &mut dyn BufRead| -> Option<String> {
        let mut w = Vec::new();
        loop {
            let mut b = [0u8; 1];
            src.read_exact(&mut b).ok()?;
            if b[0].is_ascii_whitespace() {
                if !w.is_empty() {
                    return String::from_utf8(w).ok();
                }
            } else {
                w.push(b[0]);
            }
        }
    };
    if word(src)? != "P6" {
        return None;
    }
    let (w, h): (usize, usize) = (word(src)?.parse().ok()?, word(src)?.parse().ok()?);
    if word(src)? != "255" || w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    let mut pixels = vec![0u8; w * h * 3];
    src.read_exact(&mut pixels).ok()?;
    Some((w, h, pixels))
}

/// The box of cells a picture of `w` by `h` pixels is drawn in: as big
/// as fits in `cols` by `rows`, and never stretched. `cell` is the size
/// of one cell in pixels.
pub fn fit(w: usize, h: usize, cell: (u16, u16), cols: u16, rows: u16) -> (u16, u16) {
    let (cw, ch) = (cell.0.max(1) as usize, cell.1.max(1) as usize);
    let (w, h) = (w.max(1), h.max(1));
    let high = (cols as usize * cw * h).div_ceil(w * ch);
    if high <= rows as usize {
        return (cols, high.max(1) as u16);
    }
    let wide = rows as usize * ch * w / (h * cw);
    (wide.clamp(1, cols as usize) as u16, rows)
}

/// Whole pictures out of the encoder's stream.
///
/// H.264 arrives as pieces called NAL units, each after a start code of
/// `00 00 01`. Two of the kinds hold picture data, and one picture's
/// worth of them belongs together. So a new picture starts at the second
/// picture-data unit we meet, and everything before it is one frame.
///
/// That shows the end of a picture only when the next one begins, and
/// on a still screen none does. So a picture nothing has followed for a
/// moment is whole too.
pub struct Pictures {
    src: mpsc::Receiver<Vec<u8>>,
    buf: Vec<u8>,
    ended: bool,
}

/// How long the encoder may be quiet before the picture in hand counts
/// as whole. It writes a picture in one go, so this is a long time.
const QUIET: Duration = Duration::from_millis(30);

impl Pictures {
    /// The reading is done on a thread of its own, since only a wait on
    /// a channel can give up after a while.
    pub fn new(mut src: impl Read + Send + 'static) -> Pictures {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = vec![0u8; 64 * 1024];
            while let Ok(n) = src.read(&mut chunk) {
                if n == 0 || tx.send(chunk[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Pictures { src: rx, buf: Vec::with_capacity(64 * 1024), ended: false }
    }

    /// The next whole picture, or nothing once the camera has stopped.
    pub fn next_picture(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(cut) = boundary(&self.buf) {
                let rest = self.buf.split_off(cut);
                let frame = std::mem::replace(&mut self.buf, rest);
                return Some(frame);
            }
            if self.ended {
                return if self.buf.is_empty() { None } else { Some(std::mem::take(&mut self.buf)) };
            }
            let more = if units(&self.buf).any(|(_, kind)| kind == 1 || kind == 5) {
                match self.src.recv_timeout(QUIET) {
                    Err(mpsc::RecvTimeoutError::Timeout) => return Some(std::mem::take(&mut self.buf)),
                    more => more.ok(),
                }
            } else {
                self.src.recv().ok()
            };
            match more {
                Some(bytes) => self.buf.extend(bytes),
                None => self.ended = true,
            }
        }
    }
}

/// Each unit in the buffer: where its start code begins, and what kind
/// it is. The low five bits of the byte after the start code say that:
/// 1 and 5 are picture data, 7 and 8 the settings that come before a
/// keyframe, 9 a mark between two pictures.
fn units(buf: &[u8]) -> impl Iterator<Item = (usize, u8)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < buf.len() {
            let head = match (buf[i], buf[i + 1], buf[i + 2], buf[i + 3]) {
                (0, 0, 1, _) => i + 3,
                (0, 0, 0, 1) => i + 4,
                _ => {
                    i += 1;
                    continue;
                }
            };
            if head >= buf.len() {
                return None;
            }
            let at = i;
            i = head;
            return Some((at, buf[head] & 0x1f));
        }
        None
    })
}

/// Where the second picture in the buffer starts, if it has arrived.
fn boundary(buf: &[u8]) -> Option<usize> {
    let mut seen = false;
    for (at, kind) in units(buf) {
        if kind == 1 || kind == 5 {
            if seen {
                return Some(at);
            }
            seen = true;
        } else if seen && matches!(kind, 6..=9) {
            // Settings, a note or a mark means the next picture has begun.
            return Some(at);
        }
    }
    None
}

/// Can a decoder start from this picture? A keyframe can: it holds a
/// unit of kind 5, or the settings that come before one.
pub fn is_key(picture: &[u8]) -> bool {
    units(picture)
        .find_map(|(_, kind)| match kind {
            5 | 7 => Some(true),
            1 => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// A picture as the decoder is given it: with a mark behind it. The
/// decoder shows a picture when it sees the next one begin, and the
/// mark is that beginning, so the picture shows at once. A mark the
/// sender put in front is left out, or there would be two in a row.
pub fn for_decoder(picture: &[u8]) -> Vec<u8> {
    let mut first = units(picture);
    let from = match (first.next(), first.next()) {
        (Some((_, 9)), Some((next, _))) => next,
        _ => 0,
    };
    let mut out = picture[from..].to_vec();
    out.extend([0, 0, 0, 1, 0x09, 0xf0]);
    out
}

/// The pieces of one picture, as they arrive.
///
/// There is no time to ask for a piece again in a call, so a picture
/// missing one is dropped the moment a newer picture starts.
pub struct Joining {
    frame: u32,
    pieces: Vec<Option<Vec<u8>>>,
    /// Pictures thrown away because a piece never came.
    pub lost: u64,
}

impl Joining {
    pub fn new() -> Joining {
        Joining { frame: 0, pieces: Vec::new(), lost: 0 }
    }

    /// Put one piece in, and give back the picture once it is whole.
    pub fn take(&mut self, frame: u32, index: u16, count: u16, chunk: Vec<u8>) -> Option<Vec<u8>> {
        if count == 0 || index >= count {
            return None;
        }
        if frame != self.frame || self.pieces.len() != count as usize {
            // A picture we were still waiting for is gone for good.
            if self.pieces.iter().any(|p| p.is_some()) {
                self.lost += 1;
            }
            self.frame = frame;
            self.pieces = vec![None; count as usize];
        }
        self.pieces[index as usize] = Some(chunk);
        if self.pieces.iter().any(|p| p.is_none()) {
            return None;
        }
        let whole = self.pieces.iter_mut().flat_map(|p| p.take().unwrap_or_default()).collect();
        self.pieces.clear();
        Some(whole)
    }
}

impl Default for Joining {
    fn default() -> Self {
        Joining::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for what the encoder writes: a start code, one byte
    /// saying what the unit is, then some payload.
    fn nal(kind: u8, len: usize) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, kind];
        v.extend(std::iter::repeat(0x42).take(len));
        v
    }

    #[test]
    fn a_keyframe_and_the_frames_after_it_come_out_one_at_a_time() {
        let mut stream = Vec::new();
        stream.extend(nal(7, 10)); // settings
        stream.extend(nal(8, 4));
        stream.extend(nal(5, 300)); // the keyframe
        stream.extend(nal(1, 40)); // two ordinary pictures
        stream.extend(nal(1, 50));
        let mut p = Pictures::new(std::io::Cursor::new(stream));
        let first = p.next_picture().expect("a keyframe");
        assert!(first.len() > 300, "the keyframe carries its settings with it");
        assert_eq!(p.next_picture().map(|f| f.len()), Some(45));
        assert_eq!(p.next_picture().map(|f| f.len()), Some(55));
        assert!(p.next_picture().is_none(), "and then the camera has stopped");
    }

    /// The encoder on a still screen: it writes one part, then nothing
    /// for a while, then the next.
    struct Slow(Vec<Vec<u8>>);

    impl Read for Slow {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let part = self.0.remove(0);
            if part.is_empty() {
                std::thread::sleep(Duration::from_millis(400));
                return self.read(buf);
            }
            buf[..part.len()].copy_from_slice(&part);
            Ok(part.len())
        }
    }

    #[test]
    fn a_picture_nothing_follows_is_sent_without_waiting_for_the_next() {
        let key: Vec<u8> = [nal(7, 10), nal(8, 4), nal(5, 300)].concat();
        let began = std::time::Instant::now();
        let mut p = Pictures::new(Slow(vec![key.clone(), Vec::new(), nal(1, 40)]));
        assert_eq!(p.next_picture(), Some(key));
        assert!(began.elapsed() < Duration::from_millis(300), "it waited for the next picture");
        assert_eq!(p.next_picture(), Some(nal(1, 40)));
        assert_eq!(p.next_picture(), None);
    }

    #[test]
    fn the_settings_alone_are_not_a_picture_yet() {
        // The settings come first and the picture data a moment later:
        // that pause must not cut the keyframe in two.
        let mut p = Pictures::new(Slow(vec![[nal(7, 10), nal(8, 4)].concat(), Vec::new(), nal(5, 300)]));
        assert_eq!(p.next_picture().map(|f| f.len()), Some(15 + 9 + 305));
    }

    #[test]
    fn a_keyframe_is_told_from_the_pictures_that_need_one() {
        assert!(is_key(&[nal(7, 10), nal(8, 4), nal(5, 300)].concat()));
        assert!(is_key(&[nal(9, 1), nal(5, 300)].concat()));
        assert!(!is_key(&nal(1, 40)));
        assert!(!is_key(&[nal(9, 1), nal(1, 40)].concat()));
        assert!(!is_key(b"no picture at all"));
    }

    #[test]
    fn the_decoder_gets_one_mark_behind_each_picture() {
        let mark = [0u8, 0, 0, 1, 0x09, 0xf0];
        assert_eq!(for_decoder(&nal(1, 40)), [nal(1, 40), mark.to_vec()].concat());
        // A sender that marks the front of its pictures: that mark goes,
        // or the decoder would get two in a row and find no picture between.
        let marked = [nal(9, 1), nal(1, 40)].concat();
        assert_eq!(for_decoder(&marked), [nal(1, 40), mark.to_vec()].concat());
    }

    #[test]
    fn the_encoder_is_told_what_each_kind_of_picture_needs() {
        let has = |args: &[String], words: &[&str]| args.windows(words.len()).any(|w| w == words);
        let chip = encoder_args("/dev/video0", true, ":0");
        assert!(has(&chip, &["-c:v", "h264_vaapi", "-async_depth", "1"]), "the chip gives each picture back at once");
        assert!(has(&chip, &["-profile:v", "constrained_baseline"]));
        assert!(has(&chip, &["-fps_mode", "passthrough"]), "no empty pictures where the gate dropped one");
        // libx264 refuses the name the chip's encoder takes.
        let soft = encoder_args("test", false, ":0");
        assert!(has(&soft, &["-c:v", "libx264"]) && has(&soft, &["-profile:v", "baseline"]));
        assert!(!soft.iter().any(|a| a.contains("vaapi") || a.contains("hwupload")));
        // The screen: grabbed from the display, and looked at for changes before it is scaled.
        let screen = encoder_args("screen", true, ":7");
        assert!(has(&screen, &["-f", "x11grab", "-framerate", "5", "-i", ":7"]));
        let chain = &screen[screen.iter().position(|a| a == "-vf").unwrap() + 1];
        assert!(chain.find("mpdecimate").unwrap() < chain.find("scale=").unwrap(), "{chain}");
        assert!(chain.ends_with("format=nv12,hwupload"));
    }

    #[test]
    fn a_decoded_picture_reads_back_with_its_size() {
        let mut stream = b"P6\n3 2\n255\n".to_vec();
        stream.extend([7u8; 18]);
        stream.extend(b"P6\n1 1\n255\n");
        stream.extend([9u8; 3]);
        stream.extend(b"P6\n4 4\n255\n"); // and the decoder stops half way
        stream.extend([1u8; 10]);
        let mut src = std::io::Cursor::new(stream);
        assert_eq!(read_picture(&mut src), Some((3, 2, vec![7u8; 18])));
        assert_eq!(read_picture(&mut src), Some((1, 1, vec![9u8; 3])));
        assert_eq!(read_picture(&mut src), None, "half a picture is no picture");
        assert_eq!(read_picture(&mut std::io::Cursor::new(b"JUNK and more".to_vec())), None);
    }

    #[test]
    fn a_picture_fills_its_box_without_being_stretched() {
        // Cells 10 wide and 20 high. A camera's 4:3 in 40 columns.
        assert_eq!(fit(480, 360, (10, 20), 40, 30), (40, 15));
        // A screen in a window 200 columns wide and 40 rows high: the
        // rows run out first, so it is narrower than the window.
        assert_eq!(fit(1152, 720, (10, 20), 200, 40), (128, 40));
        // A phone held upright.
        assert_eq!(fit(360, 640, (10, 20), 40, 20), (22, 20));
        assert_eq!(fit(0, 0, (0, 0), 40, 20), (20, 20), "nonsense in, a box out");
    }

    #[test]
    fn a_second_keyframe_starts_a_new_picture() {
        let mut stream = nal(5, 100);
        stream.extend(nal(7, 8));
        stream.extend(nal(5, 100));
        let mut p = Pictures::new(std::io::Cursor::new(stream));
        assert_eq!(p.next_picture().map(|f| f.len()), Some(105));
        assert_eq!(p.next_picture().map(|f| f.len()), Some(118));
    }

    #[test]
    fn a_picture_in_pieces_goes_back_together() {
        let mut j = Joining::new();
        assert!(j.take(7, 0, 3, vec![1, 2]).is_none());
        assert!(j.take(7, 2, 3, vec![5, 6]).is_none());
        assert_eq!(j.take(7, 1, 3, vec![3, 4]), Some(vec![1, 2, 3, 4, 5, 6]));
        assert_eq!(j.lost, 0);
    }

    #[test]
    fn a_picture_with_a_piece_missing_is_dropped_when_the_next_one_starts() {
        let mut j = Joining::new();
        assert!(j.take(1, 0, 2, vec![9]).is_none());
        assert_eq!(j.take(2, 0, 1, vec![7]), Some(vec![7]), "the next whole one arrives");
        assert_eq!(j.lost, 1, "the half picture was counted and thrown away");
    }

    #[test]
    fn nonsense_pieces_are_ignored() {
        let mut j = Joining::new();
        assert!(j.take(1, 0, 0, vec![1]).is_none(), "no pieces at all");
        assert!(j.take(1, 5, 2, vec![1]).is_none(), "a piece past the end");
    }
}
