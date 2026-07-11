//! IPv6-only multicast transport.
//!
//! All peers bind the same UDP port and join the same IPv6 multicast group. By
//! default the group is joined (and sent) on **every** IPv6-capable interface,
//! so multi-adapter hosts (Windows boxes with WSL/Hyper-V/VPN `vEthernet`
//! adapters, etc.) work without having to hand-pick an interface. Pass an
//! explicit interface index to override.

use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender};
use socket2::{Domain, Protocol, Socket, Type};

use crate::protocol::Envelope;

/// Default link-local ("ff12") transient multicast group used by this app.
pub const DEFAULT_GROUP: Ipv6Addr = Ipv6Addr::new(0xff12, 0, 0, 0, 0, 0, 0x4c41, 0x4e43);
pub const DEFAULT_PORT: u16 = 45654;

/// Enumerate IPv6-capable, non-loopback interfaces as `(index, name)`.
pub fn multicast_interfaces() -> Vec<(u32, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for ifa in ifaces {
            if ifa.is_loopback() {
                continue;
            }
            // Only interfaces that actually have an IPv6 address can carry the
            // group; that also filters out down/unconfigured adapters.
            if !matches!(ifa.addr, if_addrs::IfAddr::V6(_)) {
                continue;
            }
            if let Some(idx) = ifa.index {
                if idx != 0 && seen.insert(idx) {
                    out.push((idx, ifa.name.clone()));
                }
            }
        }
    }
    out
}

pub struct Net {
    socket: Arc<Socket>,
    group: SocketAddrV6,
    /// Interfaces we've joined / send the group out of. Grows over time as new
    /// interfaces (e.g. Wi-Fi coming up after launch) are discovered.
    send_ifaces: Arc<Mutex<Vec<u32>>>,
    /// Serialises sends (each send mutates the socket's multicast interface).
    send_guard: Mutex<()>,
    /// Incoming decoded envelopes together with the sender's socket address.
    pub incoming: Receiver<(Envelope, SocketAddr)>,
}

impl Net {
    /// Join `group` on `port`. `iface_index` 0 means "join on every IPv6
    /// interface"; a non-zero value pins a single interface.
    pub fn join(group: Ipv6Addr, port: u16, iface_index: u32) -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_only_v6(true)?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;

        // Bind to the wildcard address so we receive multicast traffic.
        let bind_addr: SocketAddr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));
        socket.bind(&bind_addr.into())?;
        socket.set_multicast_loop_v6(true)?;
        socket.set_multicast_hops_v6(4)?;

        // Which interfaces to join / send on.
        let targets: Vec<(u32, String)> = if iface_index != 0 {
            vec![(iface_index, format!("index {iface_index}"))]
        } else {
            multicast_interfaces()
        };

        let mut send_ifaces = Vec::new();
        for (idx, name) in &targets {
            match socket.join_multicast_v6(&group, *idx) {
                Ok(()) => {
                    log::info!("joined [{group}] on interface {idx} ({name})");
                    send_ifaces.push(*idx);
                }
                Err(e) => log::warn!("join [{group}] on interface {idx} ({name}) failed: {e}"),
            }
        }
        // Fallback: let the OS pick if we couldn't join anything specific.
        if send_ifaces.is_empty() {
            socket.join_multicast_v6(&group, 0)?;
            log::info!("joined [{group}] on default interface");
            send_ifaces.push(0);
        }

        let socket = Arc::new(socket);
        let group_addr = SocketAddrV6::new(group, port, 0, 0);
        let send_ifaces = Arc::new(Mutex::new(send_ifaces));

        let (tx, rx) = crossbeam_channel::unbounded();
        spawn_reader(socket.clone(), tx);

        // In auto mode, keep re-scanning: interfaces (Wi-Fi, VPN, tethering)
        // frequently appear *after* launch, especially on mobile.
        if iface_index == 0 {
            spawn_iface_monitor(socket.clone(), group, send_ifaces.clone());
        }

        Ok(Net {
            socket,
            group: group_addr,
            send_ifaces,
            send_guard: Mutex::new(()),
            incoming: rx,
        })
    }

    /// Send one envelope to the group on every joined interface. Thread-safe.
    pub fn send(&self, env: &Envelope) -> io::Result<()> {
        let bytes = env.encode();
        let group_ip = *self.group.ip();
        let port = self.group.port();
        // Link-local/interface-local scope needs the outgoing interface encoded
        // in the destination scope id; wider scopes don't.
        let scope = group_ip.segments()[0] & 0x000f;
        let link_scoped = scope <= 2;

        let ifaces = self.send_ifaces.lock().unwrap().clone();
        let _guard = self.send_guard.lock().unwrap();
        let mut any = false;
        let mut last_err = None;
        for &idx in &ifaces {
            let _ = self.socket.set_multicast_if_v6(idx);
            let scope_id = if link_scoped { idx } else { 0 };
            let dst: SocketAddr = SocketAddrV6::new(group_ip, port, 0, scope_id).into();
            match self.socket.send_to(&bytes, &dst.into()) {
                Ok(_) => any = true,
                Err(e) => last_err = Some(e),
            }
        }
        if any {
            Ok(())
        } else {
            Err(last_err.unwrap_or_else(|| io::Error::other("no interfaces to send on")))
        }
    }

    pub fn group(&self) -> SocketAddrV6 {
        self.group
    }
}

/// Periodically join the group on any newly-appeared interfaces.
fn spawn_iface_monitor(socket: Arc<Socket>, group: Ipv6Addr, send_ifaces: Arc<Mutex<Vec<u32>>>) {
    thread::Builder::new()
        .name("iface-monitor".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            for (idx, name) in multicast_interfaces() {
                let already = send_ifaces.lock().unwrap().contains(&idx);
                if already {
                    continue;
                }
                match socket.join_multicast_v6(&group, idx) {
                    Ok(()) => {
                        log::info!("joined [{group}] on new interface {idx} ({name})");
                        send_ifaces.lock().unwrap().push(idx);
                    }
                    Err(e) => log::debug!("join on interface {idx} ({name}): {e}"),
                }
            }
        })
        .expect("spawn iface monitor");
}

fn spawn_reader(socket: Arc<Socket>, tx: Sender<(Envelope, SocketAddr)>) {
    thread::Builder::new()
        .name("net-reader".into())
        .spawn(move || {
            let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 65535];
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        // SAFETY: the OS wrote `n` initialised bytes.
                        let data =
                            unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n) };
                        if let Some(env) = Envelope::decode(data) {
                            let from: SocketAddr = match from.as_socket() {
                                Some(s) => s,
                                None => continue,
                            };
                            if tx.send((env, from)).is_err() {
                                break; // receiver gone; app is shutting down
                            }
                        }
                    }
                    Err(e) => {
                        if e.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        log::error!("net reader error: {e}");
                        break;
                    }
                }
            }
        })
        .expect("spawn net reader");
}
