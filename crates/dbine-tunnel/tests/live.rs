//! Against real SSH servers (two `dbine-test-*` containers) and the test
//! PostgreSQL: `DBINE_TEST_SSH_KEY=<path to the test key> cargo test -p dbine-tunnel -- --ignored`.
//!
//! - dbine-test-ssh on 127.0.0.1:25022, user dbine / pw, the key's public half authorized;
//! - dbine-test-ssh-bastion on 127.0.0.1:25023, same user, reaches dbine-test-ssh:22;
//! - PostgreSQL reachable from them at host.docker.internal:25010.
//! The key's passphrase is `frase123`.

use dbine_tunnel::{open, Auth, Error, Hop, Spec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn hop(host: &str, port: u16) -> Hop {
    Hop { host: host.into(), port, user: "dbine".into() }
}

fn spec(hops: Vec<Hop>, auth: Auth, trusted: Vec<String>) -> Spec {
    Spec { hops, auth, target_host: "host.docker.internal".into(), target_port: 25010, trusted }
}

/// The fingerprint the tunnel reports for an unknown server.
async fn fingerprint(h: Hop) -> String {
    match open(&spec(vec![h], Auth::Password("pw".into()), vec![])).await {
        Err(Error::UnknownHost { fingerprint, .. }) => fingerprint,
        Err(e) => panic!("expected an unknown host, got {e}"),
        Ok(_) => panic!("an unknown host was accepted"),
    }
}

/// PostgreSQL answers an SSLRequest with one byte, 'S' or 'N'.
async fn reaches_postgres(port: u16) {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f]).await.unwrap();
    let mut b = [0u8; 1];
    s.read_exact(&mut b).await.unwrap();
    assert!(b[0] == b'S' || b[0] == b'N', "{b:?}");
}

#[tokio::test]
#[ignore]
async fn tunnels_through_ssh_to_postgres() {
    let key = std::env::var("DBINE_TEST_SSH_KEY").expect("DBINE_TEST_SSH_KEY");
    let server = fingerprint(hop("127.0.0.1", 25022)).await;
    assert!(server.starts_with("SHA256:"), "{server}");

    // Password.
    let t = open(&spec(vec![hop("127.0.0.1", 25022)], Auth::Password("pw".into()), vec![server.clone()])).await.unwrap();
    reaches_postgres(t.local_port()).await;
    reaches_postgres(t.local_port()).await; // several connections through one tunnel
    assert!(t.is_alive());
    drop(t);

    // A wrong password.
    let e = open(&spec(vec![hop("127.0.0.1", 25022)], Auth::Password("mal".into()), vec![server.clone()])).await.err().unwrap();
    assert!(matches!(e, Error::Auth { .. }), "{e}");

    // A key with its passphrase; without it; with a wrong one.
    let with = |p: Option<&str>| Auth::Key { path: key.clone().into(), passphrase: p.map(String::from) };
    let t = open(&spec(vec![hop("127.0.0.1", 25022)], with(Some("frase123")), vec![server.clone()])).await.unwrap();
    reaches_postgres(t.local_port()).await;
    let e = open(&spec(vec![hop("127.0.0.1", 25022)], with(None), vec![server.clone()])).await.err().unwrap();
    assert!(e.to_string().contains("frase"), "{e}");
    let e = open(&spec(vec![hop("127.0.0.1", 25022)], with(Some("otra")), vec![server.clone()])).await.err().unwrap();
    assert!(matches!(e, Error::Key { .. }), "{e}");

    // Through the bastion: each server's key is checked on its own.
    let bastion = fingerprint(hop("127.0.0.1", 25023)).await;
    let chain = vec![hop("127.0.0.1", 25023), hop("dbine-test-ssh", 22)];
    let inner = match open(&spec(chain.clone(), Auth::Password("pw".into()), vec![bastion.clone()])).await {
        Err(Error::UnknownHost { host, fingerprint, .. }) => {
            assert_eq!(host, "dbine-test-ssh");
            fingerprint
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("the second server's key wasn't checked"),
    };
    assert_eq!(inner, server, "same container, same key");
    let t = open(&spec(chain, with(Some("frase123")), vec![bastion, inner])).await.unwrap();
    reaches_postgres(t.local_port()).await;

    // A database that isn't there is an error when opening, not later.
    let mut bad = spec(vec![hop("127.0.0.1", 25022)], Auth::Password("pw".into()), vec![server]);
    bad.target_port = 1;
    let e = open(&bad).await.err().unwrap();
    assert!(matches!(e, Error::Forward { .. }), "{e}");
}
