//! The wire, and the relay that stands in the middle of it.
//!
//! Two people at home are both behind a router that will not accept a
//! call from outside. So neither dials the other: both send to a relay
//! on a machine with a real address, and the relay passes packets on.
//! It keeps no recording, and it cannot open what it passes on: speech,
//! pictures and text are sealed at one end and opened at the other. All
//! it reads is the head of a packet, and the room there is twelve
//! letters made from the room's name, never the name.
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
pub const TEXT: u8 = 7;

/// The longest name anyone can take.
pub const NAME_MAX: usize = 20;
/// A room is padded to this, so every header is the same length.
pub const ROOM_LEN: usize = 12;
/// Enough for one 20 ms frame of Opus with room to spare.
pub const PACKET_MAX: usize = 1200;
/// The start of every packet: what it is, which room, and who sent it.
/// This much the relay reads. The rest of a packet is sealed.
pub const HEAD: usize = 1 + ROOM_LEN + NAME_MAX;
/// What sealing adds: twelve bytes no other packet has had, and sixteen
/// that prove nothing in the packet was changed on the way.
pub const NONCE: usize = 12;
pub const TAG: usize = 16;

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

/// The start of a packet. `room` is what the relay may see of the room:
/// twelve letters made from its name, never the name.
pub fn head(kind: u8, room: &str, name: &str) -> Vec<u8> {
    let mut p = vec![kind];
    p.extend(fixed(room, ROOM_LEN));
    p.extend(fixed(name, NAME_MAX));
    p
}

/// The three packets that are a head and nothing more.
pub fn join(room: &str, name: &str) -> Vec<u8> {
    head(JOIN, room, name)
}

pub fn leave(room: &str, name: &str) -> Vec<u8> {
    head(LEAVE, room, name)
}

pub fn ping(room: &str, name: &str) -> Vec<u8> {
    head(PING, room, name)
}

/// Who sent a packet.
pub fn sender(buf: &[u8]) -> String {
    buf.get(1 + ROOM_LEN..HEAD).map(unfixed).unwrap_or_default()
}

/// A frame of speech, before it is sealed: a count so the far end can
/// tell a late packet from a lost one, and the encoded sound.
pub fn speech(seq: u32, opus: &[u8]) -> Vec<u8> {
    let mut p = seq.to_le_bytes().to_vec();
    p.extend(opus);
    p
}

/// A piece of a picture, before it is sealed: which picture this is,
/// which piece of it, and how many pieces there are. A picture is far
/// too big for one packet, so it goes in pieces.
pub fn picture(frame: u32, index: u16, count: u16, chunk: &[u8]) -> Vec<u8> {
    let mut p = frame.to_le_bytes().to_vec();
    p.extend(index.to_le_bytes());
    p.extend(count.to_le_bytes());
    p.extend(chunk);
    p
}

/// The most of a picture that fits in one packet.
pub const CHUNK_MAX: usize = PACKET_MAX - HEAD - NONCE - TAG - 8;

/// A line of text, before it is sealed: a number of its own, so the
/// far end shows it once however many times it arrives, and the text.
pub fn line(id: u32, text: &str) -> Vec<u8> {
    let mut p = id.to_le_bytes().to_vec();
    let mut end = text.len().min(LINE_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    p.extend(text[..end].bytes());
    p
}

/// The longest line of text that fits in one packet.
pub const LINE_MAX: usize = PACKET_MAX - HEAD - NONCE - TAG - 4;

/// What a client made of a packet that arrived.
pub enum In {
    Speech { from: String, seq: u32, opus: Vec<u8> },
    Picture { from: String, frame: u32, index: u16, count: u16, chunk: Vec<u8> },
    Line { from: String, id: u32, text: String },
    Who(Vec<String>),
    Other,
}

/// Read what was inside a packet of `kind` from `from`, once its seal
/// is open.
pub fn read(kind: u8, from: String, body: &[u8]) -> In {
    let n = |at: usize| u32::from_le_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
    match kind {
        AUDIO if body.len() > 4 => In::Speech { from, seq: n(0), opus: body[4..].to_vec() },
        VIDEO if body.len() > 8 => In::Picture {
            from,
            frame: n(0),
            index: u16::from_le_bytes([body[4], body[5]]),
            count: u16::from_le_bytes([body[6], body[7]]),
            chunk: body[8..].to_vec(),
        },
        TEXT if body.len() > 4 => In::Line { from, id: n(0), text: String::from_utf8_lossy(&body[4..]).into_owned() },
        _ => In::Other,
    }
}

/// The list of who is in the room, which the relay sends unsealed: it
/// made the list itself.
pub fn who(buf: &[u8]) -> In {
    if buf.first() != Some(&WHO) {
        return In::Other;
    }
    let names = buf[1..]
        .chunks(NAME_MAX)
        .filter(|c| c.len() == NAME_MAX)
        .map(unfixed)
        .filter(|s| !s.is_empty())
        .collect();
    In::Who(names)
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
/// room, and is forgotten. It is sealed with a key the relay never
/// has, so the relay never hears what was said.
fn pass_on(rooms: &Rooms, sock: &UdpSocket, buf: &[u8], from: Where) {
    if buf.len() < HEAD {
        return;
    }
    let kind = buf[0];
    // Anything else is not ours: the internet knocks on every open port.
    if !(JOIN..=TEXT).contains(&kind) || kind == WHO {
        return;
    }
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

    if kind == AUDIO || kind == VIDEO || kind == TEXT {
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
        match read(AUDIO, "geir".into(), &speech(77, &[9, 8, 7])) {
            In::Speech { from, seq, opus } => {
                assert_eq!(from, "geir");
                assert_eq!(seq, 77);
                assert_eq!(opus, vec![9, 8, 7]);
            }
            _ => panic!("a frame of speech came back as something else"),
        }
    }

    #[test]
    fn the_head_says_who_sent_it() {
        let p = head(AUDIO, "a1b2c3d4e5f6", "geir");
        assert_eq!(p.len(), HEAD);
        assert_eq!(sender(&p), "geir");
        assert_eq!(sender(&[AUDIO, 1, 2]), "", "too short to name anyone");
    }

    #[test]
    fn the_room_list_reads_back() {
        let mut list = vec![WHO];
        list.extend(fixed("geir", NAME_MAX));
        list.extend(fixed("alice", NAME_MAX));
        match who(&list) {
            In::Who(names) => assert_eq!(names, vec!["geir", "alice"]),
            _ => panic!("the room list came back as something else"),
        }
    }

    #[test]
    fn a_piece_of_a_picture_reads_back_as_it_was_sent() {
        match read(VIDEO, "geir".into(), &picture(12, 1, 4, &[3, 3, 3])) {
            In::Picture { from, frame, index, count, chunk } => {
                assert_eq!((from.as_str(), frame, index, count), ("geir", 12, 1, 4));
                assert_eq!(chunk, vec![3, 3, 3]);
            }
            _ => panic!("a piece of a picture came back as something else"),
        }
        assert!(CHUNK_MAX > 1000, "a piece big enough to be sent on its own fits in a packet");
    }

    #[test]
    fn a_line_of_text_reads_back_and_a_long_one_is_cut_between_letters() {
        match read(TEXT, "geir".into(), &line(3, "blåbær at https://example.org")) {
            In::Line { from, id, text } => assert_eq!((from.as_str(), id, text.as_str()), ("geir", 3, "blåbær at https://example.org")),
            _ => panic!("a line of text came back as something else"),
        }
        let long = "å".repeat(LINE_MAX);
        let body = line(1, &long);
        assert!(body.len() <= 4 + LINE_MAX);
        assert!(std::str::from_utf8(&body[4..]).is_ok(), "never half a letter");
    }

    #[test]
    fn the_relay_passes_on_what_is_sealed_and_drops_what_is_not_ours() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
        let hears = |kind: u8| {
            let (tx, rx) = std::sync::mpsc::channel();
            let (ta, _ra) = std::sync::mpsc::channel();
            let room = format!("room{kind}");
            pass_on(&rooms, &sock, &head(JOIN, &room, "alice"), Where::Web(1, tx));
            pass_on(&rooms, &sock, &head(JOIN, &room, "geir"), Where::Web(2, ta.clone()));
            while rx.try_recv().is_ok() {}
            let mut p = head(kind, &room, "geir");
            p.extend([1, 2, 3]);
            pass_on(&rooms, &sock, &p, Where::Web(2, ta));
            rx.try_recv().ok()
        };
        for kind in [AUDIO, VIDEO, TEXT] {
            assert_eq!(hears(kind).map(|p| p[0]), Some(kind), "kind {kind} goes to the other one in the room");
        }
        // A knock from the internet: not one of ours, so no room is made.
        let before = rooms.lock().unwrap().len();
        let mut knock = b"OPTIONS sip:100@198.51.100.7 SIP/2.0 and more of the same".to_vec();
        knock.resize(60, b' ');
        pass_on(&rooms, &sock, &knock, Where::Udp("127.0.0.1:9".parse().unwrap()));
        assert_eq!(rooms.lock().unwrap().len(), before);
    }

    #[test]
    fn rubbish_is_ignored_rather_than_believed() {
        assert!(matches!(read(AUDIO, String::new(), &[]), In::Other));
        assert!(matches!(read(AUDIO, String::new(), &[1, 2, 3, 4]), In::Other), "a count and no sound");
        assert!(matches!(read(200, String::new(), &[1, 2, 3, 4, 5, 6, 7, 8, 9]), In::Other), "a kind nobody sends");
        assert!(matches!(who(&[]), In::Other));
    }
}
