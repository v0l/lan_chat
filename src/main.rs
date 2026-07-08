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
    let args = parse_args();
    let peer_id: u64 = rand::random();

    let net = match Net::join(args.group, args.port, args.iface) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Failed to join IPv6 multicast group [{}]:{}", args.group, args.port);
            eprintln!("  {e}");
            eprintln!("Hint: ensure the interface has IPv6 enabled. Try --iface INDEX");
            eprintln!("      (see `ip -6 addr`; index from `ip link`).");
            std::process::exit(1);
        }
    };

    println!(
        "Joined [{}]:{} as \"{}\" (peer {:016x})",
        args.group, args.port, args.name, peer_id
    );

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
        Box::new(move |_cc| Ok(Box::new(ChatApp::new(peer_id, name, net)))),
    )
}
