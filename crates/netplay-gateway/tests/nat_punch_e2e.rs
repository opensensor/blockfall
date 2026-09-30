//! NAT-translation E2E for the host-side punch (field fix, 2026-09-30).
//!
//! Simulates a router that rewrites the host's source port (public port ≠
//! game port): the host's control leg registers with an advertised
//! `game_port` that is deliberately **dead** (the port a pre-fix relay would
//! forward to), while the actual game-side data flows from a *separate*
//! socket bound to a different port — exactly what a NAT does to the game
//! socket's traffic. The guest connect must succeed through the punched
//! address, and a room whose host never punches must keep dropping the guest
//! (the non-vacuity control: that is the pre-fix routing).

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

/// Reserve an OS port and release it — used as the *advertised* game port
/// nothing will ever listen on (the dead mapping a NAT never opened).
fn dead_port() -> u16 {
    let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("probe bind");
    let port = sock.local_addr().expect("probe addr").port();
    drop(sock);
    port
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

#[test]
fn nat_rewritten_host_ports_still_connect_after_punch() {
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

        // Legs: host control 127.0.0.2, post-NAT game socket ALSO 127.0.0.2
        // (the IP the gateway attributes; the port differs — the simulated
        // rewrite), guest 127.0.0.3, non-vacuity host 127.0.0.4.
        let host_ctrl = UdpSocket::bind(("127.0.0.2", 0)).expect("host ctrl bind");
        let host_game = UdpSocket::bind(("127.0.0.2", 0)).expect("host game bind");
        let guest = UdpSocket::bind(("127.0.0.3", 0)).expect("guest bind");
        let silent = UdpSocket::bind(("127.0.0.4", 0)).expect("silent bind");
        for sock in [&host_ctrl, &host_game, &guest, &silent] {
            sock.set_read_timeout(Some(Duration::from_millis(1_500)))
                .expect("read timeout");
        }

        // ── Room ONE: advertised game_port is DEAD; the host punches from
        // its game socket (separate port = the NAT rewrite).
        let code = *b"NATP3";
        let advertised = dead_port();
        host_ctrl
            .send_to(
                &wire::encode(&Frame::Register {
                    code,
                    game_port: advertised,
                }),
                ctrl_addr,
            )
            .expect("register");
        assert!(matches!(
            recv_frame(&host_ctrl),
            Some((_, Frame::Ack { .. }))
        ));
        let (_, frame) = recv_frame(&host_ctrl).expect("*V after *A");
        let punched_vport = match frame {
            Frame::VirtualPort { vport, .. } => vport,
            other => panic!("expected *V, got {other:?}"),
        };
        let vaddr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), punched_vport);

        // The guest looks the room up and starts connecting (its connect
        // request arrives at the relay BEFORE the host has punched — the
        // frame lands in the relay and is forwarded to whatever address the
        // host has taught the room so far).
        guest
            .send_to(&wire::encode(&Frame::Lookup { code }), ctrl_addr)
            .expect("lookup");
        let (_, frame) = recv_frame(&guest).expect("*F");
        let vport = match frame {
            Frame::Found { vport, .. } => vport,
            other => panic!("expected *F, got {other:?}"),
        };
        assert_eq!(vport, punched_vport);

        // Host punches (one datagram from the game socket — the field-fix
        // flow). Then the guest's connect attempt pins the slot and must be
        // forwarded to the PUNCHED port, not the dead advertised one.
        host_game.send_to(b"\x2b", vaddr).expect("host punch");
        thread::sleep(Duration::from_millis(80));
        guest
            .send_to(b"\x00connect-request", vaddr)
            .expect("guest connect");
        let (src, bytes) = recv_bytes(&host_game).expect("punched host must receive the guest");
        assert_eq!(bytes, b"\x00connect-request");
        assert_eq!(src.port(), vport, "forwards arrive from the relay vport");
        // The answer path back: host_game → relay → guest.
        host_game
            .send_to(b"\x03connect-ok", vaddr)
            .expect("host answer");
        let (src, bytes) = recv_bytes(&guest).expect("guest must receive the host answer");
        assert_eq!(bytes, b"\x03connect-ok");
        assert_eq!(src.port(), vport);

        // ── Room TWO (non-vacuity): identical setup, NO punch — the guest
        // is routed to the advertised (dead) port and gets nowhere, the
        // pre-fix behavior. Proves room ONE passed because of the punch.
        let code2 = *b"NATP4";
        let advertised2 = dead_port();
        silent
            .send_to(
                &wire::encode(&Frame::Register {
                    code: code2,
                    game_port: advertised2,
                }),
                ctrl_addr,
            )
            .expect("register 2");
        assert!(matches!(recv_frame(&silent), Some((_, Frame::Ack { .. }))));
        assert!(matches!(
            recv_frame(&silent),
            Some((_, Frame::VirtualPort { .. }))
        ));
        let guest2 = UdpSocket::bind(("127.0.0.5", 0)).expect("guest2 bind");
        guest2
            .set_read_timeout(Some(Duration::from_millis(600)))
            .expect("read timeout");
        guest2
            .send_to(&wire::encode(&Frame::Lookup { code: code2 }), ctrl_addr)
            .expect("lookup 2");
        let (_, frame) = recv_frame(&guest2).expect("*F 2");
        let vport2 = match frame {
            Frame::Found { vport, .. } => vport,
            other => panic!("expected *F, got {other:?}"),
        };
        let vaddr2: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), vport2);
        guest2
            .send_to(b"\x00unpunched", vaddr2)
            .expect("guest2 connect");
        // The room's silent host has a real socket — but on the wrong port;
        // nothing may be delivered to the only place that could observe it.
        assert!(
            recv_bytes(&silent).is_none(),
            "a never-punched room must still route to the dead advertised port"
        );
        // And the guest's own slot pin happened (a second source is *B) —
        // the bytes were forwarded, just into the NAT void, exactly like the
        // pre-fix relay.
        guest2
            .send_to(&wire::encode(&Frame::Lookup { code: code2 }), ctrl_addr)
            .expect("lookup 2 re");
        assert!(matches!(
            recv_frame(&guest2),
            Some((_, Frame::Found { .. }))
        ));

        stop.store(true, Ordering::Relaxed);
        let _ = server.join();
    });
}
