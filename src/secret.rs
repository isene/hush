//! The lock on the call.
//!
//! What is said, shown or typed is sealed before it leaves and opened
//! only at the far end. The key is made from the room's name, so
//! whoever knows the name is in the call, and nobody else can listen:
//! not the relay, and not the networks in between.
//!
//! The relay never gets the name. It gets twelve letters made from it,
//! which is enough to put the same callers in the same room.
//!
//! A name that is easy to guess makes a key that is easy to guess.
//! Making a key takes 600,000 rounds of hashing, which slows a guesser
//! down and does not stop one. A room nobody would guess is the lock.
//!
//! AES-256-GCM seals a packet, and PBKDF2 with SHA-256 makes the key.
//! Both are in every browser, so the web page does the same.

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use hush::net;
use sha2::Sha256;
use std::sync::atomic::{AtomicU32, Ordering};

/// Rounds of hashing between a room's name and its key.
const ROUNDS: u32 = 600_000;
/// The same for everyone, so the same name gives the same key.
const SALT: &[u8] = b"hush room v1";

pub struct Lock {
    cipher: Aes256Gcm,
    /// What the relay sees in place of the room's name.
    pub room: String,
    /// Eight bytes of chance picked for this call, and a count after
    /// them: together the twelve bytes no other packet has had. The
    /// count lasts 4,000 million packets, months of talking on end.
    mine: [u8; 8],
    count: AtomicU32,
}

fn hmac(key: &[u8], what: &[u8]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
    m.update(what);
    m.finalize().into_bytes().into()
}

impl Lock {
    /// The lock of the room called `name`. This is the slow part, on
    /// purpose, and it runs once a call.
    pub fn for_room(name: &str) -> Lock {
        let mut master = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(name.as_bytes(), SALT, ROUNDS, &mut master);
        // Two things from the one secret, and neither gives the other.
        let key = hmac(&master, b"key");
        let room = hmac(&master, b"room")[..6].iter().map(|b| format!("{b:02x}")).collect();
        let mut mine = [0u8; 8];
        OsRng.fill_bytes(&mut mine);
        Lock {
            cipher: Aes256Gcm::new_from_slice(&key).expect("the key is 32 bytes"),
            room,
            mine,
            count: AtomicU32::new(0),
        }
    }

    /// A packet: `head` as it is, then `body` sealed. The relay routes
    /// by the head, so it stays readable, but a head that was changed
    /// on the way spoils the seal like anything else.
    pub fn seal(&self, head: &[u8], body: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; net::NONCE];
        nonce[..8].copy_from_slice(&self.mine);
        nonce[8..].copy_from_slice(&self.count.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        let sealed = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: body, aad: head })
            .expect("a packet is far smaller than the most AES-GCM seals");
        let mut out = Vec::with_capacity(head.len() + nonce.len() + sealed.len());
        out.extend_from_slice(head);
        out.extend_from_slice(&nonce);
        out.extend(sealed);
        out
    }

    /// What was sealed in a packet. Nothing, when it was sealed with
    /// another room's key or changed on the way.
    pub fn open(&self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.len() < net::HEAD + net::NONCE + net::TAG {
            return None;
        }
        let (head, rest) = packet.split_at(net::HEAD);
        let (nonce, sealed) = rest.split_at(net::NONCE);
        self.cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: sealed, aad: head }).ok()
    }

    /// Read a packet that arrived: the relay's own list as it is, the
    /// rest through the lock.
    pub fn read(&self, packet: &[u8]) -> net::In {
        match packet.first() {
            Some(&net::WHO) => net::who(packet),
            Some(&kind) => match self.open(packet) {
                Some(body) => net::read(kind, net::sender(packet), &body),
                None => net::In::Other,
            },
            None => net::In::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// Making a lock is slow on purpose, so the tests share two.
    fn kitchen() -> &'static Lock {
        static L: OnceLock<Lock> = OnceLock::new();
        L.get_or_init(|| Lock::for_room("kitchen"))
    }

    fn cellar() -> &'static Lock {
        static L: OnceLock<Lock> = OnceLock::new();
        L.get_or_init(|| Lock::for_room("cellar"))
    }

    #[test]
    fn what_is_sealed_opens_again_in_the_same_room_only() {
        let head = net::head(net::AUDIO, &kitchen().room, "geir");
        let packet = kitchen().seal(&head, &net::speech(7, &[1, 2, 3]));
        assert_eq!(packet.len(), net::HEAD + net::NONCE + 7 + net::TAG);
        assert_eq!(kitchen().open(&packet), Some(net::speech(7, &[1, 2, 3])));
        // A second caller in the same room has the same key.
        assert_eq!(Lock::for_room("kitchen").open(&packet), Some(net::speech(7, &[1, 2, 3])));
        assert_eq!(cellar().open(&packet), None, "another room's key opens nothing");
        match kitchen().read(&packet) {
            net::In::Speech { from, seq, opus } => assert_eq!((from.as_str(), seq, opus), ("geir", 7, vec![1, 2, 3])),
            _ => panic!("a frame of speech came back as something else"),
        }
    }

    #[test]
    fn a_packet_changed_on_the_way_opens_as_nothing() {
        let head = net::head(net::TEXT, &kitchen().room, "geir");
        let packet = kitchen().seal(&head, &net::line(1, "the code is 4711"));
        for at in [0, 5, net::HEAD - 1, net::HEAD + 3, net::HEAD + net::NONCE + 2, packet.len() - 1] {
            let mut bent = packet.clone();
            bent[at] ^= 1;
            assert_eq!(kitchen().open(&bent), None, "byte {at} changed");
        }
        assert_eq!(kitchen().open(&packet[..net::HEAD + 5]), None, "cut short");
        assert!(matches!(kitchen().read(&[]), net::In::Other));
    }

    #[test]
    fn nothing_readable_is_left_and_no_two_seals_are_alike() {
        let head = net::head(net::TEXT, &kitchen().room, "geir");
        let body = net::line(1, "the code is 4711");
        let (a, b) = (kitchen().seal(&head, &body), kitchen().seal(&head, &body));
        assert!(!a.windows(4).any(|w| w == b"4711"), "the text does not show in the packet");
        assert_ne!(a[net::HEAD..], b[net::HEAD..], "the same line sealed twice differs");
        assert_ne!(a[net::HEAD..net::HEAD + net::NONCE], b[net::HEAD..net::HEAD + net::NONCE]);
    }

    /// The web page makes the same key with the browser's own tools, and
    /// this is the number both have to arrive at.
    #[test]
    fn the_relay_gets_twelve_letters_and_never_the_name() {
        assert_eq!(kitchen().room.len(), net::ROOM_LEN);
        assert!(kitchen().room.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(kitchen().room, cellar().room);
        assert_eq!(kitchen().room, "ba239e05ae2c");
    }
}
