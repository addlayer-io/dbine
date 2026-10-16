//! A phone and a DBine on this machine that only know each other's iroh id,
//! through iroh's public relays: the "outside the local network" path,
//! end to end with the channel's handshake. Needs internet:
//! `cargo run -p dbine-link --features iroh --example remote`.
//! (A plain program, not a test: under the test harness iroh's network
//! report doesn't complete on some machines.)

use dbine_link::net::{dial, endpoint, home_relay, id_of, new_secret, next, Relays};
use dbine_link::{accept, connect, Accepted, Keys, Remote};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let secret = new_secret();
    let desk = endpoint(secret, &Relays::Public, true).await.expect("DBine's endpoint");
    let phone_ep = endpoint(new_secret(), &Relays::Public, false).await.expect("the phone's endpoint");
    tokio::time::timeout(Duration::from_secs(25), desk.online()).await.expect("DBine reached a relay");
    let desktop = Keys::generate().unwrap();
    let phone = Keys::generate().unwrap();
    let known = phone.public.clone();
    let d2 = desktop.clone();
    let desk2 = desk.clone();
    let server = tokio::spawn(async move {
        let s = next(&desk2).await.unwrap().unwrap();
        let Accepted::Connected(mut ch) = accept(s, &d2, None, |k| k == known.as_slice()).await.unwrap() else { panic!("not a paired phone") };
        let got = ch.recv().await.unwrap();
        ch.send(&got).await.unwrap();
        let _ = ch.recv().await;
    });
    let started = std::time::Instant::now();
    let remote = Remote { node: id_of(secret), relays: Vec::new() };
    let s = dial(&phone_ep, &remote).await.expect("dial DBine by its id");
    let mut ch = connect(s, &desktop.public, &phone).await.expect("handshake");
    ch.send(b"desde cualquier red").await.unwrap();
    assert_eq!(ch.recv().await.unwrap(), b"desde cualquier red");
    println!("OK in {:?} through {:?}", started.elapsed(), home_relay(&desk));
    drop(ch);
    let _ = server.await;
    desk.close().await;
    phone_ep.close().await;
}
