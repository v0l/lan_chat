//! IPv6-only multicast transport.
//!
//! All peers bind the same UDP port, join the same IPv6 multicast group, and
//! exchange [`Envelope`] datagrams. No servers, no IPv4.

use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{Receiver, Sender};
use socket2::{Domain, Protocol, Socket, Type};

use crate::protocol::Envelope;

/// Default link-local ("ff02") transient multicast group used by this app.
pub const DEFAULT_GROUP: Ipv6Addr = Ipv6Addr::new(0xff12, 0, 0, 0, 0, 0, 0x4c41, 0x4e43);
pub const DEFAULT_PORT: u16 = 45654;

pub struct Net {
    socket: Arc<Socket>,
    group: SocketAddrV6,
    /// Incoming decoded envelopes together with the sender's socket address.
    pub incoming: Receiver<(Envelope, SocketAddr)>,
}

impl Net {
    /// Join `group`%`iface_index` on `port`. `iface_index` 0 means "let the OS
    /// pick the default interface".
    pub fn join(group: Ipv6Addr, port: u16, iface_index: u32) -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_only_v6(true)?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;

        // Bind to the wildcard address so we receive multicast traffic.
        let bind_addr: SocketAddr = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));
        socket.bind(&bind_addr.into())?;

        // Join the multicast group and enable loopback so multiple instances
        // on one host (and our own keepalives) work during testing.
        socket.join_multicast_v6(&group, iface_index)?;
        socket.set_multicast_loop_v6(true)?;
        socket.set_multicast_hops_v6(4)?;
        if iface_index != 0 {
            socket.set_multicast_if_v6(iface_index)?;
        }

        let socket = Arc::new(socket);
        let group_addr = SocketAddrV6::new(group, port, 0, iface_index);

        let (tx, rx) = crossbeam_channel::unbounded();
        spawn_reader(socket.clone(), tx);

        Ok(Net { socket, group: group_addr, incoming: rx })
    }

    /// Send one envelope to the multicast group.
    pub fn send(&self, env: &Envelope) -> io::Result<()> {
        let bytes = env.encode();
        let dst: SocketAddr = self.group.into();
        self.socket.send_to(&bytes, &dst.into())?;
        Ok(())
    }

    pub fn group(&self) -> SocketAddrV6 {
        self.group
    }
}

fn spawn_reader(socket: Arc<Socket>, tx: Sender<(Envelope, SocketAddr)>) {
    thread::Builder::new()
        .name("net-reader".into())
        .spawn(move || {
            // Uninitialised receive buffer, as required by socket2's recv_from.
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
                        // Transient errors: keep going; fatal: stop.
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
