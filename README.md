# hush

<img src="img/hush.svg" align="right" width="150">

**A voice call that sends nothing while you are quiet.**

![Rust](https://img.shields.io/badge/language-Rust-orange) ![Unlicense](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux-blue) ![Stay Amazing](https://img.shields.io/badge/Stay-Amazing-important)

Two people join a room by name and talk. Each end sends twenty milliseconds of Opus whenever someone is actually speaking, and not one byte when nobody is. Part of the [Fe₂O₃ Rust terminal suite](https://github.com/isene/fe2o3).

On an ordinary conversation each end talks less than half the time, and says nothing during the pauses inside its own sentences. The line is idle for most of the call, and the screen shows you what share of it carried your voice.

## Using it

Started with no room, it shows the rooms you have been in and asks which one. That is how a launcher starts it.

```bash
hush                      # pick a room from the ones you have used
hush kitchen              # join the room called kitchen
hush kitchen -n Geir      # under a name the others will see
hush kitchen -s host:port # through a particular relay
hush kitchen --no-aec     # without the echo canceller
```

| Key | Does |
|---|---|
| `m` | Mute, and unmute |
| `q` | Hang up |

## The echo

Your speaker plays the far end's voice, your microphone hears it, and a call with no answer to that sends it straight back. The far end then hears itself, half a second late.

PipeWire ships the canceller that browsers use, and hush borrows it for the length of a call.

- It is loaded when the call starts and unloaded when the call ends, so nothing of it runs in between.
- A call left behind by a killed hush is cleared away by the next one.
- `--no-aec` leaves it out, and so does naming a device with `--mic` or `--out`.
- Without PipeWire hush falls back to `arecord` and `aplay`, and then you want headphones.

Headphones are still the safer answer on a laptop with loud speakers.

## The picture

No camera runs unless you ask for one.

```bash
hush kitchen --video             # from /dev/video0
hush kitchen --video /dev/video2 # from another camera
hush kitchen --video test        # a made-up moving picture, no camera needed
```

`v` stops the camera and starts it again. Stopping kills it, so the light goes out and nothing is encoded.

The same idea as the sound gate runs here. Frames that look like the one before are thrown away before the encoder sees them, so a person sitting still sends nothing.

- A still scene at 480x360: one picture, 259 bytes, for four seconds.
- A moving scene: around 140 kbit/s.
- Encoded on the graphics chip where there is one, so the processor stays cold. Measured over a call: the sending end used 14% of one core, the receiving end 2%.
- A picture is bigger than a packet, so it goes in pieces. A piece that never arrives costs that one picture, and the next whole one replaces it.

The far end's picture is drawn in real pixels, which needs a terminal that shows them (glass, kitty, WezTerm). Anywhere else the call is sound only.

## In a browser

The same rooms, from a phone or any machine with no hush on it.

Open <https://isene.com/hush/>, type the room and a name, and you are in the call with whoever is there in a terminal.

- The browser gives the microphone, the camera, Opus, H.264 and echo cancellation. None of it has to be built.
- The same gate runs there: nothing goes out while you are quiet, and a still picture sends nothing.
- A browser cannot send UDP, so it reaches the relay over a WebSocket instead. The packets are the same bytes.
- It needs WebCodecs, which Chrome, Edge and Safari 17 have.

The page is one file, `web/index.html`. Serve it anywhere, as long as the relay is reachable at `ws` beside it.

## The relay

Two people at home are each behind a router that will not accept a call from outside. So neither dials the other. Both send to a relay on a machine with a real address, and the relay passes the packets on.

```bash
hush-relay 7777           # on a machine both ends can reach
```

It listens twice: UDP on that port for the terminal app, and TCP one port up for browsers. Put a web server in front of the second one, so a browser gets it as `wss://`.

The relay is a binary of its own. It needs nothing but Rust, and never touches the sound libraries.

```bash
cargo build --release --no-default-features --bin hush-relay
```

Check out [crust](https://github.com/isene/crust) and
[glow](https://github.com/isene/glow) beside this repo first. The caller
needs them, and Cargo wants the folders there even when the relay is
built without them.

A relay built before pictures existed passes sound only, so rebuild it.

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
