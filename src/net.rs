//! The wire, and the relay that stands in the middle of it.
//!
//! Two people at home are both behind a router that will not accept a
//! call from outside. So neither dials the other: both send to a relay
//! on a machine with a real address, and the relay passes packets on.
//! It keeps no recording and reads nothing but the room name.
//!
//! Every packet is small and stands alone. There is no session to set
//! up and nothing to tear down, so a call survives a change of network
//! without noticing.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// What a packet is. The first byte of every packet.
pub const JOIN: u8 = 1;
pub const LEAVE: u8 = 2;
pub const AUDIO: u8 = 3;
pub const PING: u8 = 4;
pub const WHO: u8 = 5;

/// The longest name anyone can take.
pub const NAME_MAX: usize = 20;
/// A room name is padded to this, so every header is the same length.
pub const ROOM_LEN: usize = 12;
/// Enough for one 20 ms frame of Opus with room to spare.
pub const PACKET_MAX: usize = 1200;

/// A name or room, cut to length and padded, so the header never moves.
pub fn fixed(s: &str, n: usize) -> Vec<u8> {
    let mut v: Vec<u8> = s.bytes().take(n).collect();
    v.resize(n, 0);
    v
}

/// The text back out of a padded field.
pub fn unfixed(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_end_matches('\0').to_string()
}

/// A packet on its way out.
pub fn join(room: &str, name: &str) -> Vec<u8> {
    let mut p = vec![JOIN];
    p.extend(fixed(room, ROOM_LEN));
    p.extend(fixed(name, NAME_MAX));
    p
}

pub fn leave(room: &str, name: &str) -> Vec<u8> {
    let mut p = join(room, name);
    p[0] = LEAVE;
    p
}

pub fn ping(room: &str, name: &str) -> Vec<u8> {
    let mut p = join(room, name);
    p[0] = PING;
    p
}

/// A frame of speech: the room, who is speaking, a count so the far end
/// can tell a late packet from a lost one, and the encoded sound.
pub fn audio(room: &str, name: &str, seq: u32, opus: &[u8]) -> Vec<u8> {
    let mut p = vec![AUDIO];
    p.extend(fixed(room, ROOM_LEN));
    p.extend(fixed(name, NAME_MAX));
    p.extend(seq.to_le_bytes());
    p.extend(opus);
    p
}

/// Where the sound starts in an AUDIO packet.
pub const AUDIO_HEAD: usize = 1 + ROOM_LEN + NAME_MAX + 4;

/// What a client made of a packet that arrived.
pub enum In {
    Speech { from: String, seq: u32, opus: Vec<u8> },
    Who(Vec<String>),
    Other,
}

/// Read a packet a client received.
pub fn parse(buf: &[u8]) -> In {
    if buf.is_empty() {
        return In::Other;
    }
    match buf[0] {
        AUDIO if buf.len() > AUDIO_HEAD => {
            let from = unfixed(&buf[1 + ROOM_LEN..1 + ROOM_LEN + NAME_MAX]);
            let seq = u32::from_le_bytes([
                buf[AUDIO_HEAD - 4],
                buf[AUDIO_HEAD - 3],
                buf[AUDIO_HEAD - 2],
                buf[AUDIO_HEAD - 1],
            ]);
            In::Speech { from, seq, opus: buf[AUDIO_HEAD..].to_vec() }
        }
        WHO => {
            let names = buf[1..]
                .chunks(NAME_MAX)
                .filter(|c| c.len() == NAME_MAX)
                .map(unfixed)
                .filter(|s| !s.is_empty())
                .collect();
            In::Who(names)
        }
        _ => In::Other,
    }
}

struct Member {
    addr: SocketAddr,
    name: String,
    seen: Instant,
}

/// Run as the relay. Nothing is stored, nothing is written down: a
/// packet comes in, goes out to the others in the same room, and is
/// forgotten.
pub fn relay(port: u16) -> std::io::Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", port))?;
    println!("hush relay listening on {port}");
    let mut rooms: HashMap<String, Vec<Member>> = HashMap::new();
    let mut buf = [0u8; PACKET_MAX];
    let mut swept = Instant::now();
    loop {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if n < 1 + ROOM_LEN + NAME_MAX {
            continue;
        }
        let kind = buf[0];
        let room = unfixed(&buf[1..1 + ROOM_LEN]);
        let name = unfixed(&buf[1 + ROOM_LEN..1 + ROOM_LEN + NAME_MAX]);
        if room.is_empty() {
            continue;
        }
        let members = rooms.entry(room.clone()).or_default();

        if kind == LEAVE {
            members.retain(|m| m.addr != from);
        } else {
            // Any packet at all counts as being here, so a talker never
            // has to announce itself twice.
            match members.iter_mut().find(|m| m.addr == from) {
                Some(m) => {
                    m.seen = Instant::now();
                    m.name = name.clone();
                }
                None => {
                    members.push(Member { addr: from, name: name.clone(), seen: Instant::now() });
                    println!("{room}: {name} joined ({} here)", members.len());
                }
            }
        }

        if kind == AUDIO {
            for m in members.iter() {
                if m.addr != from {
                    let _ = sock.send_to(&buf[..n], m.addr);
                }
            }
        } else {
            // Tell everyone who is in the room, so each end can show it.
            let mut who = vec![WHO];
            for m in members.iter() {
                who.extend(fixed(&m.name, NAME_MAX));
            }
            for m in members.iter() {
                let _ = sock.send_to(&who, m.addr);
            }
        }

        // Anyone who has not been heard from in half a minute has gone.
        if swept.elapsed() > Duration::from_secs(5) {
            swept = Instant::now();
            for (r, ms) in rooms.iter_mut() {
                let before = ms.len();
                ms.retain(|m| m.seen.elapsed() < Duration::from_secs(30));
                if ms.len() != before {
                    println!("{r}: {} left ({} here)", before - ms.len(), ms.len());
                }
            }
            rooms.retain(|_, ms| !ms.is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_survives_the_round_trip() {
        assert_eq!(unfixed(&fixed("geir", NAME_MAX)), "geir");
        assert_eq!(fixed("geir", NAME_MAX).len(), NAME_MAX);
        let long = "a".repeat(40);
        assert_eq!(unfixed(&fixed(&long, NAME_MAX)).len(), NAME_MAX, "a long name is cut, not refused");
    }

    #[test]
    fn a_frame_of_speech_reads_back_as_it_was_sent() {
        let p = audio("kitchen", "geir", 77, &[9, 8, 7]);
        match parse(&p) {
            In::Speech { from, seq, opus } => {
                assert_eq!(from, "geir");
                assert_eq!(seq, 77);
                assert_eq!(opus, vec![9, 8, 7]);
            }
            _ => panic!("a frame of speech came back as something else"),
        }
    }

    #[test]
    fn the_room_list_reads_back() {
        let mut who = vec![WHO];
        who.extend(fixed("geir", NAME_MAX));
        who.extend(fixed("alice", NAME_MAX));
        match parse(&who) {
            In::Who(names) => assert_eq!(names, vec!["geir", "alice"]),
            _ => panic!("the room list came back as something else"),
        }
    }

    #[test]
    fn rubbish_is_ignored_rather_than_believed() {
        assert!(matches!(parse(&[]), In::Other));
        assert!(matches!(parse(&[AUDIO]), In::Other), "a packet too short to hold a frame");
        assert!(matches!(parse(&[200, 1, 2]), In::Other), "a kind nobody sends");
    }
}
