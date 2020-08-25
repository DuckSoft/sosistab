use std::convert::TryInto;
use std::io::prelude::*;
use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    env_logger::init();
    smol::run(async {
        let args: Vec<String> = std::env::args().collect();
        println!("{:?}", args);
        let pubkey_bts: [u8; 32] = hex::decode(&args[2])
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap();
        let session = sosistab::connect(
            smol::unblock!((&args[1]).to_socket_addrs().unwrap().next().unwrap()),
            x25519_dalek::PublicKey::from(pubkey_bts),
        )
        .await
        .unwrap();
        println!("session established!");
        let session = sosistab::mux::Multiplex::new(session);
        {
            let connection = session.open_conn().await.unwrap();
            for msgcount in 0u64.. {
                //connection.recv_raw_bytes().await.unwrap();
                eprintln!(
                    "got {:?}, count {}",
                    connection.recv_raw_bytes().await.unwrap().len(),
                    msgcount
                );
            }
        }
        eprintln!("dropped!");
        smol::Timer::new(Duration::from_secs(10)).await;
        // for msgno in 0u64.. {
        //     // let line = smol::unblock!({
        //     //     let stdin = std::io::stdin();
        //     //     let mut lines = stdin.lock().lines();
        //     //     lines.next().unwrap().unwrap()
        //     // });
        //     let line = [0; 1024];
        //     connection
        //         .send_raw_bytes(bytes::Bytes::copy_from_slice(&line))
        //         .await
        //         .unwrap();
        //     //smol::Timer::new(std::time::Duration::from_millis(100)).await;
        // }
    })
}
