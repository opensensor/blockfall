//! One loopback integration test (gateway-plan.md G1 acceptance): a real
//! gateway thread pairing two real UDP sockets through a port-0-allocated
//! control port and an ephemeral virtual data range. The pure state machine
//! carries all semantics; this proves the socket plumbing end-to-end
//! (nonblocking drains, in-socket replies on the relay port, 5-tuple
//! continuity on both legs).

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use netplay_gateway::room::{Gateway, GatewayConfig, PortAllocator, VClock};
use netplay_gateway::wire::{self, Frame};

struct SockAlloc {
    sockets: HashMap<u16, UdpSocket>,
}

impl PortAllocator for SockAlloc {
    fn bind(&mut self, port: u16) -> Result<(), ()> {
        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).map_err(|_| ())?;
        sock.set_nonblocking(true).map_err(|_| ())?;
        self.sockets.insert(port, sock);
        Ok(())
    }
    fn close(&mut self, port: u16) {
        self.sockets.remove(&port);
    }
}

#[test]
fn loopback_pair_two_sockets_through_gateway_thread() {
    let ctrl = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("ctrl bind");
    let ctrl_addr = ctrl.local_addr().expect("ctrl addr");
    ctrl.set_nonblocking(true).expect("ctrl nonblocking");
    let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("probe bind");
    let data_base = probe.local_addr().expect("probe addr").port();
    drop(probe);

    let config = GatewayConfig {
        control_port: ctrl_addr.port(),
        data_port_start: data_base,
        data_ports: 8,
        ..GatewayConfig::default()
    };
    let mut gw = Gateway::new(
        config,
        SockAlloc {
            sockets: HashMap::new(),
        },
    );
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        let server = scope.spawn(|| {
            let start = std::time::Instant::now();
            let mut buf = [0u8; 2048];
            while !stop.load(Ordering::Relaxed) {
                let now = VClock(start.elapsed().as_millis() as u64);
                loop {
                    match ctrl.recv_from(&mut buf) {
                        Ok((n, src)) => {
                            for (dst, reply) in gw.on_packet(ctrl_addr.port(), src, &buf[..n], now)
                            {
                                let _ = ctrl.send_to(&reply, dst);
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::WouldBlock | ErrorKind::Interrupted
                            ) =>
                        {
                            break
                        }
                        Err(_) => break,
                    }
                }
                for port in gw.allocator().sockets.keys().copied().collect::<Vec<_>>() {
                    loop {
                        let received = gw
                            .allocator()
                            .sockets
                            .get(&port)
                            .map(|sock| sock.recv_from(&mut buf));
                        match received {
                            Some(Ok((n, src))) => {
                                let replies = gw.on_packet(port, src, &buf[..n], now);
                                if let Some(sock) = gw.allocator().sockets.get(&port) {
                                    for (dst, reply) in replies {
                                        let _ = sock.send_to(&reply, dst);
                                    }
                                }
                            }
                            Some(Err(e))
                                if matches!(
                                    e.kind(),
                                    ErrorKind::WouldBlock | ErrorKind::Interrupted
                                ) =>
                            {
                                break
                            }
                            _ => break,
                        }
                    }
                }
                let _ = gw.on_tick(now);
                thread::sleep(Duration::from_millis(20));
            }
        });

        let code = *b"RPBK2"; // 5 chars, alphabet-clean (no I L O 0 1)
                              // Host and guest MUST use distinct loopback addresses: the relay
                              // distinguishes host/guest legs per-IP (on the WAN they differ).
        let host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("host bind");
        let guest = UdpSocket::bind(("127.0.0.2", 0)).expect("guest bind");
        for sock in [&host, &guest] {
            sock.set_read_timeout(Some(Duration::from_millis(2_500)))
                .expect("read timeout");
        }

        // Register + lookup.
        let host_port = host.local_addr().expect("host addr").port();
        host.send_to(
            &wire::encode(&Frame::Register {
                code,
                game_port: host_port,
            }),
            ctrl_addr,
        )
        .expect("register");
        assert!(matches!(recv_frame(&host), Some((_, Frame::Ack { .. }))));
        guest
            .send_to(&wire::encode(&Frame::Lookup { code }), ctrl_addr)
            .expect("lookup");
        let (_, frame) = recv_frame(&guest).expect("*F");
        let vport = match frame {
            Frame::Found { vport, host_ip, .. } => {
                assert_eq!(host_ip, [127, 0, 0, 1]);
                assert!(vport >= data_base, "{vport} vs base {data_base}");
                vport
            }
            other => panic!("expected *F, got {other:?}"),
        };
        let vaddr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), vport);

        // Guest → host (pins the guest slot on the way)…
        guest.send_to(b"\x00from-guest", vaddr).expect("g>relay");
        let (src, bytes) = recv_bytes(&host).expect("host recv");
        assert_eq!(bytes, b"\x00from-guest");
        assert_eq!(src.port(), vport, "relay must answer from its data port");
        // …host → guest through the same relay port.
        host.send_to(b"\x05from-host", vaddr).expect("h>relay");
        let (src, bytes) = recv_bytes(&guest).expect("guest recv");
        assert_eq!(bytes, b"\x05from-host");
        assert_eq!(src.port(), vport);

        // Same-IP re-lookup stays *F; a different loopback IP sees *B
        // (pairing is tracked per-IP).
        guest
            .send_to(&wire::encode(&Frame::Lookup { code }), ctrl_addr)
            .expect("relookup");
        assert!(matches!(recv_frame(&guest), Some((_, Frame::Found { .. }))));
        let third = UdpSocket::bind(("127.0.0.3", 0)).expect("third bind");
        third
            .send_to(&wire::encode(&Frame::Lookup { code }), ctrl_addr)
            .expect("third lookup");
        assert!(matches!(recv_frame(&third), Some((_, Frame::Busy { .. }))));

        // *D tears down: the code becomes *E.
        host.send_to(&wire::encode(&Frame::Release { code }), ctrl_addr)
            .expect("release");
        guest
            .send_to(&wire::encode(&Frame::Lookup { code }), ctrl_addr)
            .expect("post-release lookup");
        assert!(matches!(
            recv_frame(&guest),
            Some((_, Frame::NotFound { .. }))
        ));

        stop.store(true, Ordering::Relaxed);
        let _ = server.join();
    });
}

fn recv_bytes(sock: &UdpSocket) -> Option<(SocketAddr, Vec<u8>)> {
    let mut buf = vec![0u8; 2048];
    match sock.recv_from(&mut buf) {
        Ok((n, src)) => Some((src, buf[..n].to_vec())),
        Err(_) => None,
    }
}

fn recv_frame(sock: &UdpSocket) -> Option<(SocketAddr, Frame)> {
    let (src, bytes) = recv_bytes(sock)?;
    Some((
        src,
        wire::decode(&bytes).expect("gateway emits valid frames"),
    ))
}
