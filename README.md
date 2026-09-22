# hush

**A voice call that sends nothing while you are quiet.**

![Rust](https://img.shields.io/badge/language-Rust-orange) ![Unlicense](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux-blue) ![Stay Amazing](https://img.shields.io/badge/Stay-Amazing-important)

Two people join a room by name and talk. Each end sends twenty milliseconds of Opus whenever someone is actually speaking, and not one byte when nobody is. Part of the [Fe₂O₃ Rust terminal suite](https://github.com/isene/fe2o3).

On an ordinary conversation each end talks less than half the time, and says nothing during the pauses inside its own sentences. The line is idle for most of the call, and the screen shows you what share of it carried your voice.

## Using it

```bash
hush kitchen              # join the room called kitchen
hush kitchen -n Geir      # under a name the others will see
hush kitchen -s host:port # through a particular relay
```

| Key | Does |
|---|---|
| `m` | Mute, and unmute |
| `q` | Hang up |

**Wear headphones.** There is no echo cancellation, so without them the far end hears itself.

## The relay

Two people at home are each behind a router that will not accept a call from outside. So neither dials the other. Both send to a relay on a machine with a real address, and the relay passes the packets on.

```bash
hush-relay 7777           # on a machine both ends can reach
```

The relay is a binary of its own. It needs nothing but Rust, and never touches the sound libraries.

```bash
cargo build --release --no-default-features --bin hush-relay
```

Put its address in `~/.hush`, one line:

```
myserver.example.com:7777
```

The relay keeps nothing. A packet arrives, goes out to the others in the same room, and is forgotten.

It never decodes anything, so it never hears what you said. It holds a name and an address for thirty seconds at a time, and no longer.

## The gate

Deciding when to send is the whole app. Here is how it decides.

- **The floor is learned, not fixed.** A kitchen and an office are not equally quiet, and a fixed threshold is wrong in both.
- **It falls at once and rises slowly.** A quiet moment drops the floor straight away. Steady noise, a fan or a road, pulls it up over about ten seconds.
- **It opens instantly and closes slowly.** Three hundred milliseconds of hangover, so the end of a word is never clipped.
- **Silence sends nothing at all.** Not a small packet, not a comfort tone. Nothing.

The meter on screen shows your level with the threshold marked on it, so you can see the gate decide.

## What it costs

Speech is about 20 kbit/s while it flows. A packet a second goes out during silence, to hold the way open through both routers, and that is 33 bytes.

## Install

```bash
sudo apt install libopus-dev
cargo install --path .
```

`arecord` and `aplay` do the sound, so there is no audio library to build against beyond Opus itself.

## Files

- `~/.hush`: the relay to go through, one line of `host:port`.

## License

Public domain. Do what you like with it.
