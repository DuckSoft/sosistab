use crate::*;
use async_dup::Arc;
use bytes::Bytes;
use smol::prelude::*;
use smol::Async;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

pub async fn connect(
    server_addr: SocketAddr,
    pubkey: x25519_dalek::PublicKey,
) -> std::io::Result<Session> {
    let my_addr = smol::unblock(|| "[::]:0".to_socket_addrs().unwrap().next().unwrap()).await;
    let udp_socket = Async::<UdpSocket>::bind(my_addr)?;
    let my_long_sk = x25519_dalek::StaticSecret::new(&mut rand::thread_rng());
    let my_eph_sk = x25519_dalek::StaticSecret::new(&mut rand::thread_rng());
    // do the handshake
    let cookie = crypt::Cookie::new(pubkey);
    let init_hello = msg::HandshakeFrame::ClientHello {
        long_pk: (&my_long_sk).into(),
        eph_pk: (&my_eph_sk).into(),
        version: 1,
    };
    let mut buf = [0u8; 2048];
    for timeout_factor in (0u32..).map(|x| 2u64.pow(x)) {
        // send hello
        let init_hello = crypt::StdAEAD::new(&cookie.generate_c2s().next().unwrap())
            .pad_encrypt(&init_hello, 1300);
        udp_socket.send_to(&init_hello, server_addr).await?;
        log::trace!("sent client hello");
        // wait for response
        let res = udp_socket
            .recv_from(&mut buf)
            .or(async {
                smol::Timer::after(Duration::from_secs(timeout_factor)).await;
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out",
                ))
            })
            .await;
        match res {
            Ok((n, _)) => {
                let buf = &buf[..n];
                for possible_key in cookie.generate_s2c() {
                    let decrypter = crypt::StdAEAD::new(&possible_key);
                    let response: Option<msg::HandshakeFrame> = decrypter.pad_decrypt(buf);
                    if let Some(msg::HandshakeFrame::ServerHello {
                        long_pk,
                        eph_pk,
                        resume_token,
                    }) = response
                    {
                        log::trace!("obtained response from server");
                        if long_pk.as_bytes() != pubkey.as_bytes() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::ConnectionRefused,
                                "bad pubkey",
                            ));
                        }
                        let shared_sec =
                            crypt::triple_ecdh(&my_long_sk, &my_eph_sk, &long_pk, &eph_pk);
                        return init_session(
                            udp_socket,
                            cookie,
                            resume_token,
                            shared_sec,
                            server_addr,
                        )
                        .await;
                    }
                }
            }
            Err(err) => {
                if err.kind() == std::io::ErrorKind::TimedOut {
                    log::trace!(
                        "timed out to {} with {}s timeout; trying again",
                        server_addr,
                        timeout_factor
                    );
                    continue;
                }
                return Err(err);
            }
        }
    }
    unimplemented!()
}

async fn init_session(
    udp_socket: Async<UdpSocket>,
    cookie: crypt::Cookie,
    resume_token: Bytes,
    shared_sec: blake3::Hash,
    remote_addr: SocketAddr,
) -> std::io::Result<Session> {
    let udp_socket = Arc::new(udp_socket);
    let up_key = blake3::keyed_hash(crypt::UP_KEY, shared_sec.as_bytes());
    let dn_key = blake3::keyed_hash(crypt::DN_KEY, shared_sec.as_bytes());
    let (send_frame_out, recv_frame_out) = async_channel::bounded::<msg::DataFrame>(100);
    let (send_frame_in, recv_frame_in) = async_channel::bounded::<msg::DataFrame>(100);
    let upload_task: smol::Task<Option<()>> = {
        let udp_socket = udp_socket.clone();
        runtime::spawn(async move {
            let up_crypter = crypt::StdAEAD::new(up_key.as_bytes());
            let mut last_restok: Option<Instant> = None;
            loop {
                let resume_token = resume_token.clone();
                let df = recv_frame_out.recv().await.ok()?;
                let send_token = match last_restok {
                    None => true,
                    Some(time) => Instant::now().saturating_duration_since(time).as_secs() > 10,
                };
                if send_token {
                    log::trace!("resending resume token...");
                    let g_encrypt = crypt::StdAEAD::new(&cookie.generate_c2s().next().unwrap());
                    udp_socket
                        .send_to(
                            &g_encrypt.pad_encrypt(
                                msg::HandshakeFrame::ClientResume { resume_token },
                                1300,
                            ),
                            remote_addr,
                        )
                        .await
                        .unwrap();
                    last_restok = Some(Instant::now());
                }
                let encrypted = up_crypter.pad_encrypt(df, 1300);
                drop(udp_socket.get_ref().send_to(&encrypted, remote_addr));
            }
        })
    };
    let download_task: smol::Task<Option<()>> = {
        let udp_socket = udp_socket;
        runtime::spawn(async move {
            let dn_crypter = crypt::StdAEAD::new(dn_key.as_bytes());
            let mut buf = [0u8; 2048];
            loop {
                let (n, _) = udp_socket.recv_from(&mut buf).await.unwrap();
                if let Some(plain) = dn_crypter.pad_decrypt::<msg::DataFrame>(&buf[..n]) {
                    log::trace!("decrypted UDP message with len {}", n);
                    send_frame_in.send(plain).await.unwrap();
                }
            }
        })
    };
    let mut session = Session::new(SessionConfig {
        latency: std::time::Duration::from_millis(5),
        target_loss: 0.01,
        send_frame: send_frame_out,
        recv_frame: recv_frame_in,
    });
    session.on_drop(move || {
        drop(download_task);
        drop(upload_task);
    });
    Ok(session)
}
