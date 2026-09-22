//! The relay: the machine in the middle that neither caller has to be.
//!
//! It decodes nothing, stores nothing and writes nothing down. A packet
//! arrives, goes out to the others in the same room, and is forgotten.
//! It needs no sound library, so it builds anywhere Rust does.

fn main() {
    let mut port = 7777u16;
    for (i, a) in std::env::args().enumerate() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("hush-relay — pass packets between two people who cannot reach each other");
                println!();
                println!("Usage: hush-relay [PORT]        (default 7777)");
                println!();
                println!("Put the address in each caller's ~/.hush, as host:port.");
                return;
            }
            "-v" | "--version" => {
                println!("hush-relay {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ if i > 0 => {
                if let Ok(p) = a.parse() {
                    port = p;
                }
            }
            _ => {}
        }
    }
    if let Err(e) = hush::net::relay(port) {
        eprintln!("hush-relay: {e}");
        std::process::exit(1);
    }
}
