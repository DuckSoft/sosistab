use smol::prelude::*;
use std::convert::TryInto;
use std::io::prelude::*;
use std::net::{TcpListener, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    // println!("TEST");
    // env_logger::init();
    // println!("YOH");
    smol::block_on(async {
        // let guard = pprof::ProfilerGuard::new(1000).unwrap();
        let args: Vec<String> = std::env::args().collect();
        let pubkey_bts: [u8; 32] = hex::decode(&args.get(2).unwrap_or(
            &"52a3c5c5fdba402c46aa4d7088a7d9c742b16fede34f8f5beb788f59501b176b".to_string(),
        ))
        .unwrap()
        .as_slice()
        .try_into()
        .unwrap();
        let session = sosistab::connect(
            smol::unblock(move || {
                (&args.get(1).unwrap_or(&"54.37.130.165:12345".to_string()))
                    .to_socket_addrs()
                    .unwrap()
                    .next()
                    .unwrap()
            })
            .await,
            x25519_dalek::PublicKey::from(pubkey_bts),
        )
        .await
        .unwrap();
        println!("session established!");
        let session = Arc::new(sosistab::mux::Multiplex::new(session));
        let client_listen = smol::Async::new(TcpListener::bind("localhost:3131").unwrap()).unwrap();
        loop {
            let (client, _) = client_listen.accept().await.unwrap();
            let client = async_dup::Arc::new(client);
            let session = session.clone();
            smol::spawn(async move {
                let remote = session.open_conn().await.unwrap();
                drop(
                    smol::future::race(
                        smol::io::copy(client.clone(), remote.to_async_writer()),
                        smol::io::copy(remote.to_async_reader(), client),
                    )
                    .await,
                );
            })
            .detach();
            // if let Ok(report) = guard.report().build() {
            //     let file = std::fs::File::create("flamegraph.svg").unwrap();
            //     report.flamegraph(file).unwrap();
            // };
        }
    })
}
