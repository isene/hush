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

use std::io::Read;
use std::process::{Child, Command, Stdio};

/// What the camera sends, and how often.
pub const WIDTH: u16 = 480;
pub const HEIGHT: u16 = 360;
pub const FPS: u16 = 15;

/// The graphics chip that does the encoding, where there is one.
const RENDER: &str = "/dev/dri/renderD128";

/// The camera, as a stream of encoded pictures. `source` is a device
/// path, or the word `test` for a moving picture that needs no camera.
pub fn camera(source: &str) -> std::io::Result<Child> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error"]);
    let hardware = std::path::Path::new(RENDER).exists();
    if hardware {
        cmd.args(["-vaapi_device", RENDER]);
    }
    if source == "test" {
        // A camera paces itself. A made-up picture does not, so it has to
        // be told, or it floods the line as fast as the chip can encode.
        cmd.args(["-re", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=size={WIDTH}x{HEIGHT}:rate={FPS}"));
    } else {
        cmd.args(["-f", "v4l2", "-framerate"])
            .arg(FPS.to_string())
            .args(["-i", source]);
    }
    // Scale to what we send, drop what has not changed, then hand the
    // frame to the chip. The numbers are how different a frame has to
    // be to count as a new one; these let a face move and hold a still
    // room shut.
    let gate = format!(
        "scale={WIDTH}:{HEIGHT},mpdecimate=hi=768:lo=320:frac=0.2{}",
        if hardware { ",format=nv12,hwupload" } else { "" }
    );
    cmd.args(["-vf", &gate]);
    if hardware {
        cmd.args(["-c:v", "h264_vaapi"]);
    } else {
        cmd.args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency"]);
    }
    // No B-frames, so a picture is ready the moment it is encoded, and
    // one keyframe at the start rather than every few seconds: a still
    // scene should cost nothing, and periodic keyframes are not nothing.
    cmd.args(["-b:v", "400k", "-g", "600", "-bf", "0", "-f", "h264", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// The far end's pictures, decoded to plain pixels at the size they will
/// be drawn, so nothing has to be scaled twice.
pub fn decoder(w: usize, h: usize) -> std::io::Result<Child> {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-flags", "low_delay"])
        .args(["-f", "h264", "-i", "-", "-vf"])
        .arg(format!("scale={w}:{h}"))
        .args(["-pix_fmt", "rgb24", "-f", "rawvideo", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// Whole pictures out of the encoder's stream.
///
/// H.264 arrives as pieces called NAL units, each after a start code of
/// `00 00 01`. Two of the kinds hold picture data, and one picture's
/// worth of them belongs together. So a new picture starts at the second
/// picture-data unit we meet, and everything before it is one frame.
pub struct Pictures<R: Read> {
    src: R,
    buf: Vec<u8>,
    ended: bool,
}

impl<R: Read> Pictures<R> {
    pub fn new(src: R) -> Pictures<R> {
        Pictures { src, buf: Vec::with_capacity(64 * 1024), ended: false }
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
            let mut chunk = [0u8; 8192];
            match self.src.read(&mut chunk) {
                Ok(0) => self.ended = true,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(_) => self.ended = true,
            }
        }
    }
}

/// Where the second picture in the buffer starts, if it has arrived.
fn boundary(buf: &[u8]) -> Option<usize> {
    let mut seen = false;
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] != 0 || buf[i + 1] != 0 {
            i += 1;
            continue;
        }
        let head = if buf[i + 2] == 1 {
            i + 3
        } else if buf[i + 2] == 0 && buf[i + 3] == 1 {
            i + 4
        } else {
            i += 1;
            continue;
        };
        if head >= buf.len() {
            return None;
        }
        // The low five bits say what the unit holds. 1 and 5 are picture
        // data; 7 and 8 are the settings that come before a keyframe.
        let kind = buf[head] & 0x1f;
        if kind == 1 || kind == 5 {
            if seen {
                return Some(i);
            }
            seen = true;
        } else if seen && (kind == 7 || kind == 8) {
            // Settings again means the next keyframe has begun.
            return Some(i);
        }
        i = head;
    }
    None
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
