use smol::prelude::*;
use std::convert::TryInto;
use std::io::prelude::*;
use std::net::{TcpListener, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(not(target_env = "msvc"))]
use jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

fn main() {
    env_logger::init();
    smol::block_on(async {
        let args: Vec<String> = std::env::args().collect();
        println!("{:?}", args);
        let pubkey_bts: [u8; 32] = hex::decode(&args[2])
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap();
        let session = sosistab::connect(
            smol::unblock(move || (&args[1]).to_socket_addrs().unwrap().next().unwrap()).await,
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
        }
    })
}
