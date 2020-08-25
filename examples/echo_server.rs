use rand::prelude::*;
use std::net::UdpSocket;
fn main() {
    env_logger::init();
    let socket = UdpSocket::bind("[::]:12345").unwrap();
    let mut badrng = rand::rngs::StdRng::seed_from_u64(0);
    smol::run(async move {
        let long_sk = x25519_dalek::StaticSecret::new(&mut badrng);
        println!(
            "listening at {} with {:?}",
            socket.local_addr().unwrap(),
            hex::encode(x25519_dalek::PublicKey::from(&long_sk).as_bytes())
        );
        let listener = sosistab::Listener::listen(socket, long_sk);
        loop {
            let socket = listener.accept_session().await.unwrap();
            println!("accepted session");
            smol::Task::spawn(async move {
                let mplex = sosistab::mux::Multiplex::new(socket);
                let conn = mplex.accept_conn().await.unwrap();
                let bts = bytes::Bytes::from(vec![0u8; 1024]);
                loop {
                    let data = bts.clone();
                    conn.send_raw_bytes(data).await.unwrap();
                }
            })
            .detach()
        }
    });
}
