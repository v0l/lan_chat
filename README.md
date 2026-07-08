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
- **Notification cues** — synthesized tones on join / leave / new message.
- **Presence** — join/leave notices, live peer list, and a "speaking" indicator.
- **GUI** built with `egui`/`eframe`.

## Windows

- Release builds use the `windows` subsystem, so **no console window** appears
  (debug builds keep the console for logs).
- The app icon (`assets/icon.png`) is used for the window/taskbar, and
  `assets/icon.ico` is embedded into the `.exe` via `build.rs` + `winresource`
  so Explorer/Alt-Tab show it too.

## Logging

Uses `env_logger`. Defaults to `info` for the app and `warn` for the noisy GUI
stack. Override with `RUST_LOG`, e.g. `RUST_LOG=lan_chat=debug cargo run`.

## Architecture

| Module          | Responsibility                                        |
|-----------------|-------------------------------------------------------|
| `protocol.rs`   | Wire format (`Envelope` + `Payload`), postcard-encoded |
| `net.rs`        | IPv6 multicast socket: multi-interface join, thread-safe send, reader |
| `audio.rs`      | cpal capture/playback, resampling, AGC + equal-weight mix, cues |
| `theme.rs`      | Visual tokens, reusable `Container` card, VU meter      |
| `app.rs`        | egui UI + background engine/keepalive/mic threads       |
| `main.rs`       | CLI args, logging, window/icon bootstrap                |

Real-time work (receive/mix/presence, mic send, keepalives) runs on **dedicated
threads**, so audio and presence keep flowing even when the window is hidden
(e.g. on another workspace) and the egui update loop is paused.

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

## Networking & troubleshooting

The app uses a **link-local scope** IPv6 group (`ff12::…`), so all clients must
be on the **same link / subnet**. Link-local multicast is never forwarded across
routers — spanning separate segments needs L2 bridging (or a wider scope *and* a
multicast-routing-capable network, which most setups lack).

**Quick diagnosis** — from each machine, ping the all-nodes group on the
interface you expect to use:

```sh
ping6 -c3 ff02::1%eth0            # Linux (use your iface)
ping -6 ff02::1%<zone-id>         # Windows (zone id: netsh interface ipv6 show interfaces)
```

If the other host's link-local address **replies**, L2 multicast works and any
remaining problem is app-level (firewall / interface). If it **doesn't**, the
network is dropping multicast — work through the list below.

### Common causes

- **Firewall (most common on Windows).** WiFi is usually the *Public* profile,
  which blocks inbound UDP. Allow the app / UDP `45654`:
  ```powershell
  New-NetFirewallRule -DisplayName "LAN Chat" -Direction Inbound `
    -Protocol UDP -LocalPort 45654 -Action Allow -Profile Any
  ```
- **Wrong interface.** By default the app joins/sends on **every** IPv6-capable
  interface, so multi-adapter machines (Windows boxes with WSL/Hyper-V/VPN
  `vEthernet` adapters, etc.) work automatically. To pin a single interface,
  pass `--iface INDEX` (`ip link` / `netsh interface ipv6 show interfaces`).
- **WiFi client / AP isolation.** Disable it — it blocks device-to-device traffic.
- **MLD snooping without a querier.** This is *MLD* (IPv6), not IGMP (IPv4) — the
  IPv4 setting has no effect here. A snooping bridge with **no querier** prunes
  and drops the group. Either disable snooping (flood) or ensure a querier.

### Switch/AP notes

- **MikroTik bridge:** the `igmp-snooping` toggle covers IGMP *and* MLD. If
  `igmp-snooping=yes` you must also set `multicast-querier=yes`, otherwise it
  drops IPv6 multicast. Simplest: `/interface bridge set <b> igmp-snooping=no`.
- **TP-Link Omada:** turn **Client Isolation off**, enable **Multicast
  Enhancement** (multicast→unicast, big reliability + voice-quality win), and
  make sure no multicast/broadcast filter is blocking the group. With a separate
  AP in the chain, flooding on the upstream bridge (snooping off) is the most
  reliable option.

WiFi multicast is inherently lossy (low-rate, unacknowledged), so *Multicast
Enhancement* / multicast-to-unicast on the AP is the real fix for smooth voice.

## Test

```sh
cargo test                    # DSP/audio e2e + multicast round-trip
cargo test --test multicast   # just the two-peer group exchange
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
