# LAN Chat · IPv6 multicast

A serverless LAN chat **and** voice app in Rust. Every instance joins the same
**IPv6 multicast group** and talks directly to every other instance on the
link — no server, no discovery service, **IPv6 only**.

## Features

- **IPv6-only** UDP multicast (`set_only_v6(true)`, transient group `ff12::4c41:4e43`).
- **Text chat** broadcast to the whole group.
- **Voice** — mic capture + speaker playback via `cpal`, 48 kHz mono, ~20 ms
  frames, concurrent speakers mixed together.
- **Presence** — join/leave notices, live peer list, "speaking" indicator.
- **GUI** built with `egui`/`eframe`.

## Architecture

| Module          | Responsibility                                        |
|-----------------|-------------------------------------------------------|
| `protocol.rs`   | Wire format (`Envelope` + `Payload`), bincode-encoded |
| `net.rs`        | IPv6 multicast socket (join, send, background reader)  |
| `audio.rs`      | cpal capture/playback, resampling, mixing jitter buffer|
| `app.rs`        | egui UI, event pumping, presence/keepalive            |
| `main.rs`       | CLI args + window bootstrap                            |

Sender identity is a random per-process `peer_id`; looped-back multicast from
your own process is filtered out by that id.

## Build & run

```sh
cargo run --release -- [NAME] [--iface INDEX] [--group IPv6] [--port PORT]
```

Defaults: group `[ff12::4c41:4e43]`, port `45654`, interface `0` (auto).

Open it on two machines (or two terminals) on the same link:

```sh
cargo run --release -- alice
cargo run --release -- bob
```

Type to chat. Tick **🎤 Transmit voice** to talk.

### Choosing an interface

Link-local/transient multicast needs the right interface. If auto (`0`) doesn't
reach peers, pass an explicit index:

```sh
ip link            # find the interface index (e.g. 2: eth0)
cargo run --release -- alice --iface 2
```

## Test

```sh
cargo test --test multicast   # two peers exchange a message over the group
```

## Notes / limitations

- Voice is raw PCM (no Opus) to keep the dependency tree pure-Rust and portable;
  it's LAN-bandwidth-friendly but not WAN-optimised.
- No encryption — intended for trusted LANs.
- Linux needs ALSA dev headers (`libasound2-dev`) to build `cpal`.
