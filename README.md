# LAN Chat · IPv6 multicast

A serverless LAN chat **and** voice app in Rust. Every instance joins the same
**IPv6 multicast group** and talks directly to every other instance on the
link — no server, no discovery service, **IPv6 only**.

## Features

- **IPv6-only** UDP multicast (`set_only_v6(true)`, transient group `ff12::4c41:4e43`).
- **Text chat** broadcast to the whole group.
- **Voice** — mic capture + speaker playback via `cpal`, 48 kHz mono, ~20 ms
  frames. Each source is loudness-normalised (basic RMS-target AGC) and
  concurrent speakers are mixed with **equal weight** (averaged over active
  speakers) so simultaneous talkers stay balanced and don't clip.
- **Device selection** — pick input (mic) and output (speaker) devices at
  runtime from the sidebar, with a refresh button to rescan.
- **Level meters (VU)** — an outbound meter while you transmit, plus a per-peer
  inbound meter so you can see at a glance that you're actually receiving each
  participant's audio.
- **Themed UI** — a compact "network console" look (dark cool surfaces, hairline
  cards, a single signal-teal accent reserved for live state) built on a
  reusable `Container` card widget and a token system in `src/theme.rs`.

## Logging

Uses `env_logger`. Defaults to `info` for the app and `warn` for the noisy GUI
stack. Override with `RUST_LOG`, e.g. `RUST_LOG=lan_chat=debug cargo run`.
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

### Chat commands

Lines beginning with `/` are commands (use `//` to send a literal leading slash):

| Command | Action |
|---------|--------|
| `/name <n>` (`/nick`) | Change your display name and re-announce to peers |
| `/me <action>` | Send an emote (`* alice waves`) |
| `/mic [on\|off]` | Toggle or set voice transmission (no arg = toggle) |
| `/peers` | List peers currently on the group |
| `/clear` | Clear your local chat log |
| `/quit` (`/exit`) | Announce departure and close |
| `/help` (`/?`) | List commands |

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

## Serialization

Wire messages use [`postcard`](https://crates.io/crates/postcard) (a compact,
actively-maintained `serde` format) — bincode's final release is a maintenance
tombstone, so postcard is the recommended modern replacement.

## Notes / limitations

- Voice is raw PCM (no Opus) to keep the dependency tree pure-Rust and portable;
  it's LAN-bandwidth-friendly but not WAN-optimised.
- No encryption — intended for trusted LANs.
- Linux needs ALSA dev headers (`libasound2-dev`) to build `cpal`.
