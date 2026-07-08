//! LAN Chat — IPv6-only multicast chat & voice with a GUI.
//!
//! Usage:
//!   lan_chat [NAME] [--iface INDEX] [--group ADDR] [--port PORT]
//!
//! Every instance on the same link that joins the same group/port sees each
//! other. No server. IPv6 only.

mod app;
mod audio;
mod net;
mod protocol;
mod theme;

use std::net::Ipv6Addr;
use std::str::FromStr;

use app::ChatApp;
use net::{Net, DEFAULT_GROUP, DEFAULT_PORT};

struct Args {
    name: String,
    group: Ipv6Addr,
    port: u16,
    iface: u32,
}

fn parse_args() -> Args {
    let mut name = gethostname::gethostname().to_string_lossy().to_string();
    let mut group = DEFAULT_GROUP;
    let mut port = DEFAULT_PORT;
    let mut iface = 0u32;

    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--iface" => {
                if let Some(v) = it.next() {
                    iface = v.parse().unwrap_or(0);
                }
            }
            "--group" => {
                if let Some(v) = it.next() {
                    if let Ok(g) = Ipv6Addr::from_str(&v) {
                        group = g;
                    }
                }
            }
            "--port" => {
                if let Some(v) = it.next() {
                    port = v.parse().unwrap_or(DEFAULT_PORT);
                }
            }
            "--help" | "-h" => {
                println!(
                    "lan_chat [NAME] [--iface INDEX] [--group IPv6] [--port PORT]\n\
                     Defaults: group [{DEFAULT_GROUP}] port {DEFAULT_PORT} iface 0 (auto)"
                );
                std::process::exit(0);
            }
            other if !other.starts_with("--") => name = other.to_string(),
            _ => {}
        }
    }

    Args { name, group, port, iface }
}

fn main() -> eframe::Result<()> {
    // Logging: default to info for our crate, quieter for the noisy GUI stack.
    // Override anytime with RUST_LOG, e.g. `RUST_LOG=lan_chat=debug`.
    env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("info,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,eframe=warn,egui_winit=warn,winit=warn,naga=warn"),
    )
    .format_timestamp_millis()
    .init();

    let args = parse_args();
    let peer_id: u64 = rand::random();

    let net = match Net::join(args.group, args.port, args.iface) {
        Ok(n) => n,
        Err(e) => {
            log::error!("failed to join IPv6 multicast group [{}]:{}: {e}", args.group, args.port);
            log::error!("ensure the interface has IPv6 enabled; try --iface INDEX (see `ip -6 addr` / `ip link`)");
            std::process::exit(1);
        }
    };

    log::info!(
        "joined [{}]:{} as \"{}\" (peer {:016x})",
        args.group, args.port, args.name, peer_id
    );

    // Audio devices are a common source of "no sound": cpal uses ALSA's
    // "default", which on PipeWire/Pulse may not be your active sink. Log the
    // options so you can pick the right Output in the Voice panel.
    log::info!("output devices: {:?}", audio::list_output_devices());
    log::info!("input devices: {:?}", audio::list_input_devices());

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([760.0, 520.0])
            .with_title("LAN Chat · IPv6"),
        ..Default::default()
    };

    let name = args.name.clone();
    eframe::run_native(
        "lan_chat",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(ChatApp::new(peer_id, name, net)))
        }),
    )
}
