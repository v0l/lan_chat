//! Smoke test: two peers join the same IPv6 multicast group and exchange a
//! text message over the loopback-enabled group.

use std::time::Duration;

// Pull the binary crate's modules in by path.
#[path = "../src/protocol.rs"]
mod protocol;
#[path = "../src/net.rs"]
mod net;

use net::{Net, DEFAULT_GROUP};
use protocol::{Envelope, Payload};

#[test]
fn two_peers_exchange_text() {
    // Use a dedicated port so the test doesn't clash with a running app.
    let port = 46999;

    let a = Net::join(DEFAULT_GROUP, port, 0).expect("peer A join");
    let b = Net::join(DEFAULT_GROUP, port, 0).expect("peer B join");

    // Give the group membership a moment to settle.
    std::thread::sleep(Duration::from_millis(200));

    let msg = Envelope::new(
        1,
        Payload::Text { name: "alice".into(), body: "hello ipv6".into() },
    );
    a.send(&msg).expect("send");

    // B should receive it (loopback is enabled).
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut got = false;
    while std::time::Instant::now() < deadline {
        if let Ok((env, _)) = b.incoming.recv_timeout(Duration::from_millis(200)) {
            if let Payload::Text { name, body } = env.payload {
                assert_eq!(name, "alice");
                assert_eq!(body, "hello ipv6");
                got = true;
                break;
            }
        }
    }
    assert!(got, "peer B did not receive the multicast text within timeout");
}
