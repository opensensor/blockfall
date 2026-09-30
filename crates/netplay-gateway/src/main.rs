//! Thin relay binary: nonblocking UDP sockets + a fixed 10 ms poll tick
//! feeding the pure [`Gateway`] state machine (gateway-plan.md G1).
//!
//! std has no `poll()`; the loop instead drains every live socket to
//! `WouldBlock` each 100 ms tick. At lockstep load (~2–4 KB/s per peer)
//! that is far below socket buffer capacity, and it keeps the binary
//! dependency-free. Signals use std defaults: SIGINT/SIGTERM terminate the
//! process; the OS reclaims all sockets (no persistence — documented in
//! the crate README).

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use netplay_gateway::room::{Gateway, GatewayConfig, PortAllocator, VClock};
use netplay_gateway::wire;

/// Max datagram kept from the wire (netcode packets cap at ~1200; anything
/// larger is truncated at read — the codec rejects it, never panics).
const READ_BUF: usize = 2048;
/// Poll period: max added latency per hop, and the GC tick granularity.
/// 10 ms keeps worst-case relay latency (~2 hops ≈ 20 ms + wire) well under
/// the netplay input delay; a v0.3.1 field bug shipped 100 ms, which pushed
/// every guest input past its delayed tick on real paths (the host then
/// deterministically ran the empty-input path — guest controls looked dead).
/// The 10 ms wake-up draining a handful of nonblocking sockets is noise.
const POLL: Duration = Duration::from_millis(10);
/// Periodic summary cadence (keep logs sparse).
const SUMMARY: Duration = Duration::from_secs(60);

/// Real allocator: owns one nonblocking `UdpSocket` per virtual data port;
/// dropping the socket is the `close`.
struct SockAllocator {
    bind_ip: IpAddr,
    sockets: HashMap<u16, UdpSocket>,
}

impl SockAllocator {
    fn new(bind_ip: IpAddr) -> Self {
        Self {
            bind_ip,
            sockets: HashMap::new(),
        }
    }

    fn socket(&self, port: u16) -> Option<&UdpSocket> {
        self.sockets.get(&port)
    }

    fn ports(&self) -> Vec<u16> {
        self.sockets.keys().copied().collect()
    }
}

impl PortAllocator for SockAllocator {
    fn bind(&mut self, port: u16) -> Result<(), ()> {
        let sock = UdpSocket::bind(SocketAddr::new(self.bind_ip, port)).map_err(|e| {
            eprintln!("gateway: bind {}:{port} failed: {e}", self.bind_ip);
        })?;
        sock.set_nonblocking(true).map_err(|_| ())?;
        self.sockets.insert(port, sock);
        Ok(())
    }

    fn close(&mut self, port: u16) {
        self.sockets.remove(&port);
    }
}

#[derive(Debug)]
struct Options {
    listen: SocketAddr,
    data_start: u16,
    data_count: u16,
    idle_secs: u64,
    self_test: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 27016)),
            data_start: 27017,
            data_count: 983,
            idle_secs: 15,
            self_test: false,
        }
    }
}

fn parse_args(argv: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut i = 0;
    macro_rules! value {
        ($flag:literal) => {{
            i += 1;
            let Some(v) = argv.get(i) else {
                return Err(concat!($flag, " needs a value").to_string());
            };
            v.clone()
        }};
    }
    while i < argv.len() {
        match argv[i].as_str() {
            "--listen" => {
                let v = value!("--listen");
                opts.listen = v
                    .parse()
                    .map_err(|_| format!("--listen: expected <addr:port>, got {v}"))?;
            }
            "--data-start" => {
                let v = value!("--data-start");
                opts.data_start = v.parse().map_err(|_| format!("--data-start: {v}"))?;
            }
            "--data-count" => {
                let v = value!("--data-count");
                opts.data_count = v.parse().map_err(|_| format!("--data-count: {v}"))?;
            }
            "--idle-secs" => {
                let v = value!("--idle-secs");
                opts.idle_secs = v.parse().map_err(|_| format!("--idle-secs: {v}"))?;
            }
            "--self-test" => opts.self_test = true,
            "--help" | "-h" => return Err("HELP".to_string()),
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(opts)
}

const USAGE: &str = "\
netplay-gateway — Blockfall introduce + port-paired UDP relay

Usage: netplay-gateway [OPTIONS]
    --listen <addr:port>   control/listen address (default 0.0.0.0:27016)
    --data-start <port>    first virtual data port (default 27017)
    --data-count <n>       number of data slots (default 983)
    --idle-secs <secs>     host-idle GC window (default 15; guest slot: 10)
    --self-test            run the in-process loopback relay self-test and exit
    -h, --help             show this help

Firewall: allow inbound UDP on the whole control+data range. SIGINT/SIGTERM
terminate the process (the OS closes all sockets; nothing persists).";

/// The serve loop: drain control + every live data socket to `WouldBlock`
/// per tick, feed the pure gateway, answer from the receiving socket (the
/// peer pins the 5-tuple, so relays must reply in-socket), then GC.
fn serve(
    gw: &mut Gateway<SockAllocator>,
    control: &UdpSocket,
    control_port: u16,
    stop: &AtomicBool,
    verbose: bool,
) {
    let start = Instant::now();
    let mut buf = vec![0u8; READ_BUF];
    let mut ctrl_rx: u64 = 0;
    let mut fwd_bytes: u64 = 0;
    let mut last_summary = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let now = VClock(start.elapsed().as_millis() as u64);
        // Control port.
        loop {
            match control.recv_from(&mut buf) {
                Ok((n, src)) => {
                    ctrl_rx += 1;
                    let replies = gw.on_packet(control_port, src, &buf[..n], now);
                    send_replies(control, replies, &mut fwd_bytes);
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                    break
                }
                Err(e) => eprintln!("gateway: control recv error: {e}"),
            }
        }
        // Data ports (snapshot the table; *D during the drain above may
        // already have closed one — socket() returning None ends that sweep).
        for port in gw.allocator().ports() {
            loop {
                let received = gw
                    .allocator()
                    .socket(port)
                    .map(|sock| sock.recv_from(&mut buf));
                let received = match received {
                    Some(result) => result,
                    None => break,
                };
                match received {
                    Ok((n, src)) => {
                        let replies = gw.on_packet(port, src, &buf[..n], now);
                        if let Some(sock) = gw.allocator().socket(port) {
                            send_replies(sock, replies, &mut fwd_bytes);
                        }
                    }
                    Err(e)
                        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
                    {
                        break
                    }
                    Err(e) => eprintln!("gateway: data recv error on {port}: {e}"),
                }
            }
        }
        for event in gw.on_tick(now) {
            if verbose {
                eprintln!("gateway: gc: {event}");
            }
        }
        if verbose && last_summary.elapsed() >= SUMMARY {
            last_summary = Instant::now();
            eprintln!(
                "gateway: rooms={} ctrl_rx={} fwd_bytes={} rate_limited={}",
                gw.live_rooms(),
                ctrl_rx,
                fwd_bytes,
                gw.rate_limited(),
            );
        }
        thread::sleep(POLL);
    }
}

/// Sends the gateway's replies from the receiving socket, logging
/// collisions/exhaustion sparsely.
fn send_replies(sock: &UdpSocket, replies: Vec<(SocketAddr, Vec<u8>)>, fwd_bytes: &mut u64) {
    for (dst, reply) in replies {
        match sock.send_to(&reply, dst) {
            Ok(n) => {
                *fwd_bytes += n as u64;
                if let Ok(wire::Frame::Collision { code }) = wire::decode(&reply) {
                    eprintln!(
                        "gateway: collision on code {} from {dst}",
                        String::from_utf8_lossy(&code)
                    );
                }
            }
            Err(e) => eprintln!("gateway: send to {dst} failed: {e}"),
        }
    }
}

fn run(opts: &Options) -> std::io::Result<()> {
    let stop = AtomicBool::new(false);
    let control = UdpSocket::bind(opts.listen)?;
    control.set_nonblocking(true)?;
    let last = u32::from(opts.data_start) + u32::from(opts.data_count) - 1;
    eprintln!(
        "netplay-gateway {} — listening {} (UDP), data ports {}-{} ({} slots), host-idle {} s",
        env!("CARGO_PKG_VERSION"),
        opts.listen,
        opts.data_start,
        last,
        opts.data_count,
        opts.idle_secs,
    );
    let config = GatewayConfig {
        control_port: opts.listen.port(),
        data_port_start: opts.data_start,
        data_ports: opts.data_count,
        unpaired_idle: Duration::from_secs(opts.idle_secs),
        paired_idle: Duration::from_secs(opts.idle_secs),
        ..GatewayConfig::default()
    };
    let mut gw = Gateway::new(config, SockAllocator::new(opts.listen.ip()));
    serve(&mut gw, &control, opts.listen.port(), &stop, true);
    Ok(())
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&argv) {
        Ok(opts) => opts,
        Err(err) if err == "HELP" => {
            println!("{USAGE}");
            return;
        }
        Err(err) => {
            eprintln!("netplay-gateway: {err}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if opts.self_test {
        std::process::exit(if self_test() { 0 } else { 1 });
    }
    if let Err(e) = run(&opts) {
        eprintln!("netplay-gateway: fatal: {e}");
        std::process::exit(1);
    }
}

fn fail(step: &str) -> bool {
    println!("FAIL netplay-gateway self-test: {step}");
    false
}

/// Ephemeral loopback self-test: real sockets, real gateway loop, two UDP
/// peers paired through the relay (the scenario G4's smoke script relies
/// on). Prints PASS/FAIL; exit code matches.
fn self_test() -> bool {
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let ctrl_port = free_port(loopback);
    let data_base = free_port(loopback);
    let config = GatewayConfig {
        control_port: ctrl_port,
        data_port_start: data_base,
        data_ports: 4,
        ..GatewayConfig::default()
    };
    let control = match UdpSocket::bind((loopback, ctrl_port)) {
        Ok(sock) => sock,
        Err(e) => return fail(&format!("bind control: {e}")),
    };
    if let Err(e) = control.set_nonblocking(true) {
        return fail(&format!("nonblocking: {e}"));
    }
    let mut gw = Gateway::new(config, SockAllocator::new(loopback));
    let stop = AtomicBool::new(false);
    let ctrl_addr = SocketAddr::new(loopback, ctrl_port);

    let result = thread::scope(|scope| {
        let handle = scope.spawn(|| serve(&mut gw, &control, ctrl_port, &stop, false));
        let outcome = self_test_scenario(ctrl_addr, data_base);
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
        outcome
    });
    match result {
        Ok(()) => {
            println!("PASS netplay-gateway self-test");
            true
        }
        Err(step) => fail(&step),
    }
}

fn free_port(bind_ip: IpAddr) -> u16 {
    let sock = UdpSocket::bind(SocketAddr::new(bind_ip, 0)).expect("ephemeral probe");
    let port = sock.local_addr().expect("probe addr").port();
    drop(sock);
    port
}

/// The two-peer pairing scenario. Err carries the failing step.
fn self_test_scenario(ctrl_addr: SocketAddr, data_base: u16) -> Result<(), String> {
    let code = *b"SEFT2"; // 5 chars, alphabet-clean (no I L O 0 1)
                          // Host and guest MUST use distinct loopback addresses: the relay
                          // distinguishes host/guest legs per-IP (on the WAN they always differ).
    let host = UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("host bind: {e}"))?;
    let guest = UdpSocket::bind("127.0.0.2:0").map_err(|e| format!("guest bind: {e}"))?;
    host.set_read_timeout(Some(Duration::from_millis(2_000)))
        .map_err(|e| e.to_string())?;
    guest
        .set_read_timeout(Some(Duration::from_millis(2_000)))
        .map_err(|e| e.to_string())?;

    // Unknown code → *E before anything is registered.
    guest
        .send_to(&wire::encode(&wire::Frame::Lookup { code }), ctrl_addr)
        .map_err(|e| format!("guest lookup send: {e}"))?;
    expect_frame(&guest, |f| matches!(f, wire::Frame::NotFound { .. }), "*E")?;

    // Host registers → *A, and the field-fix *V right behind it (the host's
    // punch port). Consume both — a leftover *V would poison the data-plane
    // reads below.
    let host_port = host.local_addr().map_err(|e| e.to_string())?.port();
    host.send_to(
        &wire::encode(&wire::Frame::Register {
            code,
            game_port: host_port,
        }),
        ctrl_addr,
    )
    .map_err(|e| format!("host register send: {e}"))?;
    expect_frame(&host, |f| matches!(f, wire::Frame::Ack { .. }), "*A")?;
    let v_from_ack = match expect_frame(
        &host,
        |f| matches!(f, wire::Frame::VirtualPort { .. }),
        "*V",
    ) {
        Ok((_, data)) => match wire::decode(&data).map_err(|e| e.to_string())? {
            wire::Frame::VirtualPort { vport, .. } => vport,
            other => return Err(format!("unexpected *V payload {other:?}")),
        },
        Err(e) => return Err(e),
    };

    // Guest looks up → *F with the relay vport.
    guest
        .send_to(&wire::encode(&wire::Frame::Lookup { code }), ctrl_addr)
        .map_err(|e| format!("guest lookup send: {e}"))?;
    let (_, data) = expect_frame(&guest, |f| matches!(f, wire::Frame::Found { .. }), "*F")?;
    let vport = match wire::decode(&data).map_err(|e| e.to_string())? {
        wire::Frame::Found { vport, host_ip, .. } => {
            if host_ip != [127, 0, 0, 1] {
                return Err(format!("*F host_ip {host_ip:?}"));
            }
            if vport < data_base {
                return Err(format!("*F vport {vport} below base {data_base}"));
            }
            vport
        }
        other => return Err(format!("unexpected reply {other:?}")),
    };
    if v_from_ack != vport {
        return Err(format!(
            "*V port {v_from_ack} disagrees with *F vport {vport}"
        ));
    }
    let vaddr = SocketAddr::from(([127, 0, 0, 1], vport));

    // Host data before any guest packet: dropped (netcode dials from guest).
    host.send_to(b"\x05early", vaddr)
        .map_err(|e| format!("host early send: {e}"))?;
    // Guest data pins the guest slot and reaches the host…
    guest
        .send_to(b"\x00guest-1", vaddr)
        .map_err(|e| format!("guest send: {e}"))?;
    let (src, data) = recv_to(&host).ok_or("host did not receive guest data")?;
    if data != b"\x00guest-1" || src.port() != vport {
        return Err(format!("host got {data:?} from {src}"));
    }
    // …and host→guest flows through the same relay port.
    host.send_to(b"\x05host-1", vaddr)
        .map_err(|e| format!("host send: {e}"))?;
    let (src, data) = recv_to(&guest).ok_or("guest did not receive host data")?;
    if data != b"\x05host-1" || src.port() != vport {
        return Err(format!("guest got {data:?} from {src}"));
    }
    // Room paired: a second guest sees *B, the same guest stays *F.
    guest
        .send_to(&wire::encode(&wire::Frame::Lookup { code }), ctrl_addr)
        .map_err(|e| format!("guest relookup send: {e}"))?;
    expect_frame(&guest, |f| matches!(f, wire::Frame::Found { .. }), "*F")?;
    // A different source IP (the loopback /8 gives us one): busy. Same-IP
    // re-lookup stays *F because pairing is tracked per-IP by design.
    let third = UdpSocket::bind("127.0.0.3:0").map_err(|e| format!("third bind: {e}"))?;
    third
        .send_to(&wire::encode(&wire::Frame::Lookup { code }), ctrl_addr)
        .map_err(|e| format!("third lookup send: {e}"))?;
    expect_frame(&third, |f| matches!(f, wire::Frame::Busy { .. }), "*B")?;

    // *D tears down immediately: the code is then *E.
    host.send_to(&wire::encode(&wire::Frame::Release { code }), ctrl_addr)
        .map_err(|e| format!("host release send: {e}"))?;
    guest
        .send_to(&wire::encode(&wire::Frame::Lookup { code }), ctrl_addr)
        .map_err(|e| format!("guest post-release lookup send: {e}"))?;
    expect_frame(&guest, |f| matches!(f, wire::Frame::NotFound { .. }), "*E")?;
    Ok(())
}

/// Reads one datagram, honoring the socket's read timeout.
fn recv_to(sock: &UdpSocket) -> Option<(SocketAddr, Vec<u8>)> {
    let mut buf = vec![0u8; READ_BUF];
    match sock.recv_from(&mut buf) {
        Ok((n, src)) => Some((src, buf[..n].to_vec())),
        Err(_) => None,
    }
}

/// Reads exactly one reply (2 s read timeout ≫ the 100 ms poll tick) and
/// asserts its variant against `wanted`.
fn expect_frame(
    sock: &UdpSocket,
    wanted: impl Fn(&wire::Frame) -> bool,
    what: &str,
) -> Result<(SocketAddr, Vec<u8>), String> {
    let (src, data) = recv_to(sock).ok_or_else(|| format!("timeout waiting for {what}"))?;
    let frame = wire::decode(&data).map_err(|e| format!("undecodable reply: {e}"))?;
    if wanted(&frame) {
        Ok((src, data))
    } else {
        Err(format!("expected {what}, got {frame:?}"))
    }
}
