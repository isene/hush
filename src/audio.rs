//! Sound in, sound out, and the gate that decides whether any of it is
//! worth sending.
//!
//! The microphone and the speaker are `arecord` and `aplay`, two small
//! programs that come with ALSA. Each runs once for the length of the
//! call and streams raw samples through a pipe, so there is no sound
//! library to build against and nothing is spawned per frame.
//!
//! The gate is the point of the app. A frame of speech is about fifty
//! bytes once Opus has it. A frame of quiet is nothing at all: not a
//! small packet, not a comfort tone, nothing. On a normal call that is
//! most of the time.

use std::process::{Child, Command, Stdio};

/// Sound is handled 48,000 times a second, in frames of 20 milliseconds.
pub const RATE: usize = 48_000;
pub const FRAME: usize = RATE / 50;

/// How loud a frame is, from 0 to 1, by root mean square.
pub fn loudness(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|s| (*s as f64 / 32768.0).powi(2)).sum();
    (sum / frame.len() as f64).sqrt() as f32
}

/// The gate: is this speech, or is it the room being quiet?
///
/// It learns the quiet of the room rather than trusting a fixed number,
/// because a kitchen and an office are not equally quiet. It opens at
/// once and closes slowly, so the end of a word is never clipped.
pub struct Gate {
    /// What quiet sounds like here, learned and kept.
    floor: f32,
    /// How many frames to keep sending after the talking stops.
    hangover: u32,
    left: u32,
    /// How much louder than the floor counts as speech.
    pub factor: f32,
    /// Below this nothing is ever speech, however quiet the room.
    pub gate_min: f32,
    pub open: bool,
}

impl Gate {
    pub fn new() -> Gate {
        Gate {
            floor: 0.002,
            // Three hundred milliseconds. Shorter clips word endings.
            hangover: 15,
            left: 0,
            factor: 2.6,
            gate_min: 0.004,
            open: false,
        }
    }

    /// Decide on one frame, and learn from it.
    pub fn speaking(&mut self, level: f32) -> bool {
        let loud = level > self.gate_min && level > self.floor * self.factor;
        // The floor drops to meet a quiet moment at once, and creeps up
        // the rest of the time whatever the gate decided. That second
        // part matters: a fan or a road is loud enough to look like
        // speech at first, and without the creep the gate would stay
        // open for the whole call. It takes about ten seconds to learn
        // a room, and a single word is far too short to move it.
        if level < self.floor {
            self.floor += (level - self.floor) * 0.25;
        } else {
            self.floor *= 1.004;
        }
        self.floor = self.floor.clamp(0.0005, 0.08);
        if loud {
            self.left = self.hangover;
        } else if self.left > 0 {
            self.left -= 1;
        }
        self.open = loud || self.left > 0;
        self.open
    }

    /// What the gate currently thinks quiet sounds like.
    pub fn floor(&self) -> f32 {
        self.floor
    }
}

impl Default for Gate {
    fn default() -> Self {
        Gate::new()
    }
}

/// The microphone, as a stream of raw samples.
pub fn microphone(device: &str) -> std::io::Result<Child> {
    Command::new("arecord")
        .args(["-q", "-D", device, "-f", "S16_LE", "-r"])
        .arg(RATE.to_string())
        .args(["-c", "1", "-t", "raw", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// The speaker, as somewhere to write raw samples.
pub fn speaker(device: &str) -> std::io::Result<Child> {
    Command::new("aplay")
        .args(["-q", "-D", device, "-f", "S16_LE", "-r"])
        .arg(RATE.to_string())
        // A small buffer, because a call is worth more latency-free than
        // it is worth gap-free.
        .args(["-c", "1", "-t", "raw", "--buffer-size=4800", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// Samples as bytes, and back, in the order ALSA wants them.
pub fn to_bytes(frame: &[i16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(frame.len() * 2);
    for s in frame {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

pub fn to_samples(bytes: &[u8]) -> Vec<i16> {
    bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: f64) -> Vec<i16> {
        (0..n)
            .map(|i| ((i as f64 * 0.05).sin() * amp * 32767.0) as i16)
            .collect()
    }

    #[test]
    fn loudness_runs_from_quiet_to_loud() {
        assert_eq!(loudness(&[0i16; 100]), 0.0);
        let soft = loudness(&tone(960, 0.01));
        let loud = loudness(&tone(960, 0.5));
        assert!(loud > soft * 20.0, "a shout reads far louder than a whisper");
        assert!(loud < 1.0 && soft > 0.0);
    }

    #[test]
    fn the_gate_stays_shut_on_a_quiet_room() {
        let mut g = Gate::new();
        for _ in 0..200 {
            g.speaking(0.001);
        }
        assert!(!g.open, "a quiet room never opens the gate");
    }

    #[test]
    fn the_gate_opens_on_speech_and_holds_briefly_after() {
        let mut g = Gate::new();
        for _ in 0..100 {
            g.speaking(0.001);
        }
        assert!(g.speaking(0.2), "a loud frame opens it at once");
        let mut open_after = 0;
        for _ in 0..40 {
            if g.speaking(0.001) {
                open_after += 1;
            }
        }
        assert!(open_after >= 10 && open_after <= 20, "it holds for about three hundred milliseconds, not {open_after} frames");
        assert!(!g.open, "and then it shuts");
    }

    #[test]
    fn the_gate_learns_a_noisy_room_rather_than_shouting_over_it() {
        let mut quiet = Gate::new();
        let mut noisy = Gate::new();
        for _ in 0..600 {
            quiet.speaking(0.001);
            noisy.speaking(0.02);
        }
        assert!(noisy.floor() > quiet.floor() * 4.0, "the noisy room learned a higher floor");
        // The same voice: loud enough in a quiet room, lost in a noisy one.
        assert!(quiet.speaking(0.03), "speech carries in the quiet room");
        assert!(!noisy.speaking(0.03), "the same level is only the room next door");
    }

    #[test]
    fn samples_survive_the_round_trip_through_bytes() {
        let f = tone(960, 0.4);
        assert_eq!(to_samples(&to_bytes(&f)), f);
        assert_eq!(to_bytes(&f).len(), FRAME * 2);
    }
}
