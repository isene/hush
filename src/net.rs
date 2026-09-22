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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a packet is. The first byte of every packet.
pub const JOIN: u8 = 1;
pub const LEAVE: u8 = 2;
pub const AUDIO: u8 = 3;
pub const PING: u8 = 4;
pub const WHO: u8 = 5;
pub const VIDEO: u8 = 6;

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

/// A piece of a picture: the room, who is looking at the camera, which
/// picture this is, which piece of it, and how many pieces there are.
/// A picture is far too big for one packet, so it goes in pieces.
pub fn video(room: &str, name: &str, frame: u32, index: u16, count: u16, chunk: &[u8]) -> Vec<u8> {
    let mut p = vec![VIDEO];
    p.extend(fixed(room, ROOM_LEN));
    p.extend(fixed(name, NAME_MAX));
    p.extend(frame.to_le_bytes());
    p.extend(index.to_le_bytes());
    p.extend(count.to_le_bytes());
    p.extend(chunk);
    p
}

/// Where the picture starts in a VIDEO packet.
pub const VIDEO_HEAD: usize = 1 + ROOM_LEN + NAME_MAX + 4 + 2 + 2;

/// The most of a picture that fits in one packet.
pub const CHUNK_MAX: usize = PACKET_MAX - VIDEO_HEAD;

/// What a client made of a packet that arrived.
pub enum In {
    Speech { from: String, seq: u32, opus: Vec<u8> },
    Picture { from: String, frame: u32, index: u16, count: u16, chunk: Vec<u8> },
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
        VIDEO if buf.len() > VIDEO_HEAD => {
            let from = unfixed(&buf[1 + ROOM_LEN..1 + ROOM_LEN + NAME_MAX]);
            let n = 1 + ROOM_LEN + NAME_MAX;
            let frame = u32::from_le_bytes([buf[n], buf[n + 1], buf[n + 2], buf[n + 3]]);
            let index = u16::from_le_bytes([buf[n + 4], buf[n + 5]]);
            let count = u16::from_le_bytes([buf[n + 6], buf[n + 7]]);
            In::Picture { from, frame, index, count, chunk: buf[VIDEO_HEAD..].to_vec() }
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

/// Where a member is, and how to reach them. The terminal app sends UDP
/// packets. A browser cannot, so it comes over a WebSocket instead, and
/// sends the very same bytes.
enum Where {
    Udp(SocketAddr),
    /// A number of its own, and the queue its own thread writes out.
    Web(u64, std::sync::mpsc::Sender<Vec<u8>>),
}

struct Member {
    at: Where,
    name: String,
    seen: Instant,
}

/// What tells one member from another: an address, or a browser's own
/// number. Names do not serve, since two people may share one.
#[derive(PartialEq)]
enum Key {
    Udp(SocketAddr),
    Web(u64),
}

fn key_of(at: &Where) -> Key {
    match at {
        Where::Udp(a) => Key::Udp(*a),
        Where::Web(i, _) => Key::Web(*i),
    }
}

impl Member {
    fn key(&self) -> Key {
        key_of(&self.at)
    }

    /// Hand a packet over, whichever way this member is reached.
    fn give(&self, packet: &[u8], sock: &UdpSocket) {
        match &self.at {
            Where::Udp(a) => {
                let _ = sock.send_to(packet, a);
            }
            Where::Web(_, tx) => {
                let _ = tx.send(packet.to_vec());
            }
        }
    }
}

type Rooms = Arc<Mutex<HashMap<String, Vec<Member>>>>;

/// One packet, passed on to the rest of the room.
///
/// This is the whole of the relay. Nothing is stored and nothing is
/// written down: a packet comes in, goes out to the others in the same
/// room, and is forgotten. It is never decoded, so the relay never
/// hears what was said.
fn pass_on(rooms: &Rooms, sock: &UdpSocket, buf: &[u8], from: Where) {
    if buf.len() < 1 + ROOM_LEN + NAME_MAX {
        return;
    }
    let kind = buf[0];
    let room = unfixed(&buf[1..1 + ROOM_LEN]);
    let name = unfixed(&buf[1 + ROOM_LEN..1 + ROOM_LEN + NAME_MAX]);
    if room.is_empty() {
        return;
    }
    let key = key_of(&from);
    let mut rooms = rooms.lock().unwrap_or_else(|e| e.into_inner());
    let members = rooms.entry(room.clone()).or_default();

    if kind == LEAVE {
        members.retain(|m| m.key() != key);
    } else {
        // Any packet at all counts as being here, so a talker never has
        // to announce itself twice.
        match members.iter_mut().find(|m| m.key() == key) {
            Some(m) => {
                m.seen = Instant::now();
                m.name = name.clone();
            }
            None => {
                members.push(Member { at: from, name: name.clone(), seen: Instant::now() });
                println!("{room}: {name} joined ({} here)", members.len());
            }
        }
    }

    if kind == AUDIO || kind == VIDEO {
        // Whoever sent it is the one member it does not go back to.
        for m in members.iter().filter(|m| m.key() != key) {
            m.give(buf, sock);
        }
    } else {
        // Tell everyone who is in the room, so each end can show it.
        let mut who = vec![WHO];
        for m in members.iter() {
            who.extend(fixed(&m.name, NAME_MAX));
        }
        for m in members.iter() {
            m.give(&who, sock);
        }
    }
}

/// Anyone not heard from in half a minute has gone.
fn sweep(rooms: &Rooms) {
    let mut rooms = rooms.lock().unwrap_or_else(|e| e.into_inner());
    for (r, ms) in rooms.iter_mut() {
        let before = ms.len();
        ms.retain(|m| m.seen.elapsed() < Duration::from_secs(30));
        if ms.len() != before {
            println!("{r}: {} left ({} here)", before - ms.len(), ms.len());
        }
    }
    rooms.retain(|_, ms| !ms.is_empty());
}

/// Run as the relay: UDP for the terminal app, and a WebSocket one port
/// up for browsers, which cannot send UDP at all.
pub fn relay(port: u16) -> std::io::Result<()> {
    let sock = Arc::new(UdpSocket::bind(("0.0.0.0", port))?);
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
    let web = port + 1;
    {
        let (rooms, sock) = (rooms.clone(), sock.clone());
        std::thread::spawn(move || web_door(rooms, sock, web));
    }
    println!("hush relay listening on {port}, browsers on {web}");
    let mut buf = [0u8; PACKET_MAX];
    let mut swept = Instant::now();
    loop {
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            pass_on(&rooms, &sock, &buf[..n], Where::Udp(from));
        }
        if swept.elapsed() > Duration::from_secs(5) {
            swept = Instant::now();
            sweep(&rooms);
        }
    }
}

/// The door browsers come in by. One thread each, which is plenty for a
/// call between a handful of people.
fn web_door(rooms: Rooms, sock: Arc<UdpSocket>, port: u16) {
    let Ok(door) = std::net::TcpListener::bind(("0.0.0.0", port)) else {
        println!("nothing listening for browsers on {port}");
        return;
    };
    let count = Arc::new(std::sync::atomic::AtomicU64::new(1));
    for stream in door.incoming().flatten() {
        let (rooms, sock, count) = (rooms.clone(), sock.clone(), count.clone());
        std::thread::spawn(move || {
            let id = count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            web_member(rooms, sock, stream, id);
        });
    }
}

/// One browser, from its first packet to the moment it goes away.
///
/// A WebSocket is one socket for both directions, so this reads with a
/// short timeout and writes whatever is queued in between. That costs a
/// wake every ten milliseconds while a browser is connected, and
/// nothing at all when none is.
fn web_member(rooms: Rooms, sock: Arc<UdpSocket>, stream: std::net::TcpStream, id: u64) {
    let _ = stream.set_nodelay(true);
    let Ok(mut ws) = tungstenite::accept(stream) else {
        return;
    };
    let _ = ws.get_ref().set_read_timeout(Some(Duration::from_millis(10)));
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    loop {
        match ws.read() {
            Ok(tungstenite::Message::Binary(bytes)) => {
                pass_on(&rooms, &sock, &bytes, Where::Web(id, tx.clone()));
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
        let mut sent = false;
        while let Ok(out) = rx.try_recv() {
            if ws.write(tungstenite::Message::Binary(out)).is_err() {
                return;
            }
            sent = true;
        }
        if sent && ws.flush().is_err() {
            return;
        }
    }
    // Out of every room it was in, the moment the browser closes.
    let mut rooms = rooms.lock().unwrap_or_else(|e| e.into_inner());
    for ms in rooms.values_mut() {
        ms.retain(|m| !matches!(m.at, Where::Web(other, _) if other == id));
    }
    rooms.retain(|_, ms| !ms.is_empty());
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
    fn a_piece_of_a_picture_reads_back_as_it_was_sent() {
        let p = video("kitchen", "geir", 12, 1, 4, &[3, 3, 3]);
        match parse(&p) {
            In::Picture { from, frame, index, count, chunk } => {
                assert_eq!((from.as_str(), frame, index, count), ("geir", 12, 1, 4));
                assert_eq!(chunk, vec![3, 3, 3]);
            }
            _ => panic!("a piece of a picture came back as something else"),
        }
        assert!(CHUNK_MAX > 1000, "a piece worth sending fits in a packet");
    }

    #[test]
    fn rubbish_is_ignored_rather_than_believed() {
        assert!(matches!(parse(&[]), In::Other));
        assert!(matches!(parse(&[AUDIO]), In::Other), "a packet too short to hold a frame");
        assert!(matches!(parse(&[200, 1, 2]), In::Other), "a kind nobody sends");
    }
}
