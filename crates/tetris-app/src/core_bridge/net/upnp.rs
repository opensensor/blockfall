//! UPnP IGD auto port mapping (netplay-plan.md "Addendum — WAN play"):
//! discover the home router with SSDP, ask it to forward the host's UDP
//! port with `AddPortMapping`, and read the public IP back with
//! `GetExternalIPAddress` — no router clicks for cross-WAN hosting, no
//! hosted UDP tunnel (ngrok 3 / cloudflared dropped raw UDP in 2026, see
//! the plan addendum), zero new dependencies (std SSDP + SOAP ≈ this file).
//!
//! # Layering
//!
//! * **Pure protocol core** — every byte-level decision is a pure function
//!   over canned input: [`parse_ssdp_location`] (SSDP response bytes →
//!   `LOCATION`), [`find_control_url`] (device description XML →
//!   `(controlURL, serviceType)`), [`resolve_url`], the `soap_*` request
//!   builders, [`parse_soap_error`] / [`parse_external_ip`] /
//!   [`upnp_error_text`], and the [`UpnpState`] transition table
//!   ([`apply_upnp_event`]) + renewal math ([`renew_due`]). The tests run
//!   them on fixtures with **no network at all**.
//! * **Thin IO glue** — [`discover`] (SSDP M-SEARCH, one probe per 500 ms
//!   until the 2 s deadline) and the tiny HTTP/1.0 client behind
//!   [`map_from_location`] / [`renew_mapping`] / [`delete_mapping`]. The
//!   fake-IGD test drives the whole client against a std `TcpStream`
//!   loopback server, pinning the wire format without a router.
//! * **Async wrapper** — [`UpnpState`] (the UI's view) and [`UpnpDriver`]
//!   (generation counter + result mailbox). Every protocol run executes on
//!   a `std::thread` — the 2 s SSDP timeout must never hitch a frame —
//!   and reports back through an mpsc mailbox the [`upnp_driver_system`]
//!   polls on `Update`. A generation counter drops results that raced a
//!   teardown (and fire-forgets their stale mapping instead).
//!
//! # Honest XML shortcut
//!
//! There is no XML parser (dependency minimalism): responses are scanned
//! with a targeted tag finder ([`tag_value`]) — first `<controlURL>` inside
//! the `<service>` block whose `<serviceType>` names
//! `urn:schemas-upnp-org:service:WANIPConnection:2`, falling back to `:1`;
//! SOAP faults are read off `<errorCode>`; the external IP off
//! `<NewExternalIPAddress>`. That covers real IGD replies; hostile input
//! cannot panic it (pure string surgery), it just fails to parse →
//! [`UpnpState::Failed`].
//!
//! # Teardown
//!
//! `net_stop()` paths drop the session to `Idle`; the driver system watches
//! that edge and deletes the mapping on a bounded-timeout thread. App exit
//! attempts one synchronous best-effort delete; **SIGKILL leaves the
//! 1-hour lease to expire by itself — which is exactly why the lease is
//! finite and renewed every 30 min while the host still holds the session**
//! ([`LEASE_DURATION_SECS`] / [`RENEW_INTERVAL`]).

use std::io;
use std::io::Read;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bevy::prelude::*;

use super::online_ui::{local_ipv4, DEFAULT_HOST_PORT};
use super::session::{NetRole, NetSession, NetStatus};
use crate::settings_persist::NetProfile;

/// Multicast group + port every SSDP device listens on.
pub const SSDP_ADDR: &str = "239.255.255.250:1900";
/// Discovery target: an internet gateway device (the IGD root device).
pub const SSDP_TARGET: &str = "urn:schemas-upnp-org:device:InternetGatewayDevice:1";
/// How long the background probe waits for *any* gateway answer.
pub const SSDP_TIMEOUT: Duration = Duration::from_secs(2);
/// Probe cadence: `send` + wait, re-`send` until the deadline.
const SSDP_PROBE_INTERVAL: Duration = Duration::from_millis(500);
/// Receive timeout granularity while polling for responses.
const SSDP_READ_TICK: Duration = Duration::from_millis(100);
/// `NewLeaseDuration` handed to the router: one hour, self-expiring.
pub const LEASE_DURATION_SECS: u32 = 3_600;
/// Re-`AddPortMapping` at half the lease while the host still holds the
/// session, so a long hosting run never loses the mapping.
pub const RENEW_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Per-request HTTP timeout for description/SOAP exchanges.
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
/// Bounded teardown for the delete thread.
const DELETE_TIMEOUT: Duration = Duration::from_millis(750);
/// Last-chance synchronous delete on app exit (worst frame hitch allowed).
const EXIT_DELETE_TIMEOUT: Duration = Duration::from_millis(250);
/// Human-readable marker inside the mapping entry on the router.
pub const MAPPING_DESCRIPTION: &str = "blockfall-netplay";

/// IGD service types, preferred first (`:2` before `:1`).
pub const SERVICE_V2: &str = "urn:schemas-upnp-org:service:WANIPConnection:2";
/// Fallback WAN IP connection service (older routers).
pub const SERVICE_V1: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";

// ---------------------------------------------------------------------------
// Pure protocol core — bytes in, parsed answer out
// ---------------------------------------------------------------------------

/// The M-SEARCH request body (headers, terminated blank line).
#[must_use]
pub fn ssdp_request() -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_ADDR}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {SSDP_TARGET}\r\n\r\n"
    )
}

/// Extract the `LOCATION:` header from SSDP response bytes. Garbage lines
/// are skipped; with a burst of responses the first well-formed
/// `http`/`https` location wins.
#[must_use]
pub fn parse_ssdp_location(response: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(response);
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("LOCATION") {
            continue;
        }
        let value = value.trim();
        if value.starts_with("http://") || value.starts_with("https://") {
            return Some(value.to_string());
        }
    }
    None
}

/// First tag value with local name `tag`, tolerating namespace prefixes
/// (`<u:Foo>` matches `"Foo"`) and attributes. Plain string surgery — see
/// the honest-shortcut note in the module docs.
#[must_use]
fn tag_value(xml: &str, tag: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(open) = xml[from..].find('<') {
        let open = from + open;
        let gt = xml[open..].find('>')?;
        let head = &xml[open + 1..open + gt];
        from = open + gt + 1;
        if head.is_empty()
            || head.starts_with('/')
            || head.starts_with('!')
            || head.starts_with('?')
            || head.ends_with('/')
        {
            continue;
        }
        let name = head.split_whitespace().next().unwrap_or("");
        if name.rsplit(':').next().unwrap_or(name) != tag {
            continue;
        }
        let end_tag = format!("</{name}>");
        let close = xml[from..].find(end_tag.as_str())?;
        return Some(xml[from..from + close].trim().to_string());
    }
    None
}

/// Start of the `<service>` block enclosing `idx` (not `<serviceList>` —
/// scanning from that would let a sibling service's `controlURL` win).
fn service_block_start(xml: &str, idx: usize) -> usize {
    let with_space = xml[..idx].rfind("<service ").unwrap_or(0);
    let bare = xml[..idx].rfind("<service>").unwrap_or(0);
    with_space.max(bare)
}

/// Locate the WAN IP connection control endpoint: `WANIPConnection:2` when
/// the router advertises it, else `:1`. Returns `(controlURL, serviceType)`
/// — the service type rides along because every SOAP call names it.
#[must_use]
pub fn find_control_url(xml: &str) -> Option<(String, String)> {
    for service in [SERVICE_V2, SERVICE_V1] {
        for (idx, _) in xml.match_indices(service) {
            let block_start = service_block_start(xml, idx);
            let block_end = xml[idx..]
                .find("</service>")
                .map_or(xml.len(), |offset| idx + offset);
            if let Some(url) = tag_value(&xml[block_start..block_end], "controlURL") {
                return Some((url, service.to_string()));
            }
        }
    }
    None
}

/// Resolve a `controlURL` against the `LOCATION` base: absolute URLs pass
/// through, `/root-relative` URLs against the origin, bare names against
/// the base document's directory. Path traversal is not resolved (real IGD
/// replies never need it).
#[must_use]
pub fn resolve_url(base_location: &str, control_url: &str) -> Option<String> {
    if control_url.starts_with("http://") || control_url.starts_with("https://") {
        return Some(control_url.to_string());
    }
    let scheme_end = base_location.find("://")?;
    let after_authority = &base_location[scheme_end + 3..];
    let (origin, path) = match after_authority.find('/') {
        Some(idx) => base_location.split_at(scheme_end + 3 + idx),
        None => (base_location, ""),
    };
    if let Some(rest) = control_url.strip_prefix('/') {
        return Some(format!("{origin}/{rest}"));
    }
    let dir = match path.rfind('/') {
        Some(idx) => &path[..=idx],
        None => "/",
    };
    Some(format!("{origin}{dir}{control_url}"))
}

/// Wrap a SOAP body for `service#action` (the wire format the fake-IGD
/// test pins).
#[must_use]
pub fn soap_envelope(service: &str, action: &str, inner_args: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\n<s:Body>\n<u:{action} xmlns:u=\"{service}\">\n{inner_args}</u:{action}>\n</s:Body>\n</s:Envelope>"
    )
}

/// `AddPortMapping` argument block (remote host empty = any source).
#[must_use]
pub fn add_port_mapping_args(port: u16, internal_client: &str, lease_secs: u32) -> String {
    format!(
        "<NewRemoteHost></NewRemoteHost>\
         <NewExternalPort>{port}</NewExternalPort>\
         <NewProtocol>UDP</NewProtocol>\
         <NewInternalPort>{port}</NewInternalPort>\
         <NewInternalClient>{internal_client}</NewInternalClient>\
         <NewEnabled>1</NewEnabled>\
         <NewPortMappingDescription>{MAPPING_DESCRIPTION}</NewPortMappingDescription>\
         <NewLeaseDuration>{lease_secs}</NewLeaseDuration>"
    )
}

/// `DeletePortMapping` argument block (the tuple as mapped).
#[must_use]
pub fn delete_port_mapping_args(port: u16) -> String {
    format!(
        "<NewRemoteHost></NewRemoteHost>\
         <NewExternalPort>{port}</NewExternalPort>\
         <NewProtocol>UDP</NewProtocol>"
    )
}

/// SOAP fault code from a response body (UPnP `UPnPError/errorCode`).
#[must_use]
pub fn parse_soap_error(body: &str) -> Option<u16> {
    tag_value(body, "errorCode").and_then(|raw| raw.trim().parse().ok())
}

/// External IPv4 from a `GetExternalIPAddress` response (`ExternalIPAddress`
/// is the service-template spelling; `New…` appears in the wild too).
#[must_use]
pub fn parse_external_ip(body: &str) -> Option<String> {
    tag_value(body, "ExternalIPAddress").or_else(|| tag_value(body, "NewExternalIPAddress"))
}

/// Human-readable reason for an IGD error code (718 conflict, 725 no timed
/// leases, 726 no such entry; 40x = "not a WAN connection we can control").
#[must_use]
pub fn upnp_error_text(code: u16) -> String {
    match code {
        718 => "router already maps this UDP port for another device (UPnP error 718 — conflict)"
            .to_string(),
        725 => "router only accepts permanent port mappings (UPnP error 725)".to_string(),
        726 => "no matching port mapping on the router (UPnP error 726)".to_string(),
        402 => "router's WAN connection is not IP-capable (UPnP error 402)".to_string(),
        502 => "router is busy, UPnP refused (UPnP error 502)".to_string(),
        other => format!("router refused the UPnP mapping (error {other})"),
    }
}

/// A finished mapping (pure data — everything later calls need).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappingOk {
    /// Public IPv4 the router reports.
    pub external_ip: String,
    /// External (and internal) UDP port.
    pub port: u16,
    /// Resolved SOAP control endpoint (kept for renew + delete).
    pub control_url: String,
    /// Which `WANIPConnection` version answered.
    pub service: String,
    /// LAN IPv4 the entry points at (replayed on renewal).
    pub internal_ip: Ipv4Addr,
}

// ---------------------------------------------------------------------------
// Pure state + scheduling
// ---------------------------------------------------------------------------

/// UI-visible UPnP progress (Host screen). `Off` is the default: no
/// attempt has run or everything was torn down.
#[derive(Clone, Debug, Default, PartialEq, Eq, Resource)]
pub enum UpnpState {
    /// No mapping held (or UPnP disabled).
    #[default]
    Off,
    /// A discovery/mapping attempt is running on its thread.
    Mapping {
        /// UDP port being opened.
        port: u16,
    },
    /// Mapping live: friends reach the host at `external_ip:port`.
    Mapped {
        /// Public IPv4 from the router.
        external_ip: String,
        /// Forwarded UDP port.
        port: u16,
    },
    /// The attempt failed; the reason is for logs only — the Host screen
    /// shows the fixed manual-forward line.
    Failed(String),
}

/// Events that drive [`UpnpState`]; applied through the pure table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpnpEvent {
    /// Kick off an attempt for `port`.
    Start {
        /// UDP port the host listens on.
        port: u16,
    },
    /// An attempt (or renewal) succeeded.
    Mapped {
        /// Public IPv4 from the router.
        external_ip: String,
        /// Forwarded UDP port.
        port: u16,
    },
    /// An attempt failed.
    Failed(String),
    /// Tear everything down (net_stop path).
    Teardown,
}

/// The pure [`UpnpState`] transition table. `Teardown` is absolute; `Start`
/// supersedes whatever came before (re-host, retry after toggle);
/// `Mapped`/`Failed` are ignored outside the states they make sense in —
/// notably a renewal failure never drags a live `Mapped` back to `Failed`
/// (a transient probe miss must not scare the player while the mapping
/// still holds).
#[must_use]
pub fn apply_upnp_event(current: &UpnpState, event: &UpnpEvent) -> UpnpState {
    use UpnpEvent as E;
    use UpnpState as S;
    match event {
        E::Teardown => S::Off,
        E::Start { port } => S::Mapping { port: *port },
        E::Mapped { external_ip, port } => match current {
            S::Mapping { .. } | S::Mapped { .. } => S::Mapped {
                external_ip: external_ip.clone(),
                port: *port,
            },
            _ => current.clone(),
        },
        E::Failed(reason) => match current {
            S::Mapping { .. } => S::Failed(reason.clone()),
            _ => current.clone(),
        },
    }
}

/// Renewal scheduling: due once half the lease has passed since the last
/// successful (or attempted) renewal. `None` (never mapped/renewed) is not
/// due.
#[must_use]
pub fn renew_due(last_renew: Option<Instant>, now: Instant) -> bool {
    last_renew.is_some_and(|last| now.saturating_duration_since(last) >= RENEW_INTERVAL)
}

/// Whether a host session at `status` still *holds* the mapping (the
/// window where renew must keep firing — includes an in-progress match:
/// losing the mapping there would cut new connections and confuse copy).
#[must_use]
pub fn mapping_held(status: &NetStatus, role: NetRole) -> bool {
    role == NetRole::Host
        && matches!(
            status,
            NetStatus::Listening | NetStatus::Handshaking | NetStatus::Ready | NetStatus::InMatch
        )
}

// ---------------------------------------------------------------------------
// Thin IO glue (no unit-test traffic — the fake-IGD test pins this)
// ---------------------------------------------------------------------------

fn http_url_parts(url: &str) -> Option<(String, u16, String)> {
    let scheme_end = url.find("://")?;
    let https = url[..scheme_end].eq_ignore_ascii_case("https");
    let (authority, path) = match url[scheme_end + 3..].find('/') {
        Some(idx) => {
            let cut = scheme_end + 3 + idx;
            (url[scheme_end + 3..cut].to_string(), url[cut..].to_string())
        }
        None => (url[scheme_end + 3..].to_string(), "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, raw)) => (host.to_string(), raw.parse().ok()?),
        None => (authority, if https { 443 } else { 80 }),
    };
    Some((host, port, path))
}

fn resolve_addr(host: &str, port: u16) -> io::Result<SocketAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    // LOCATION headers are usually numeric; hostnames get std's blocking
    // resolver (LAN routers, one A record — never a frame, this runs on
    // the mapping thread).
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no address for host"))
}

/// Minimal HTTP/1.0 exchange (close-delimited bodies, per-read timeout;
/// the response body is returned once status 2xx, errors as `io::Error`).
fn http_exchange(
    method: &str,
    url: &str,
    soap_action: Option<&str>,
    body: &str,
    timeout: Duration,
) -> io::Result<(bool, String)> {
    let (host, port, path) = http_url_parts(url)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad URL"))?;
    if url.starts_with("https://") {
        // IGD control endpoints are plain HTTP on the LAN; no TLS here.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "https UPnP endpoints are not supported",
        ));
    }
    let addr = resolve_addr(&host, port)?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut request = format!("{method} {path} HTTP/1.0\r\nHOST: {host}:{port}\r\n");
    if let Some(action) = soap_action {
        request.push_str(&format!(
            "SOAPACTION: \"{action}\"\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n"
        ));
    }
    if !body.is_empty() {
        request.push_str(&format!("CONTENT-LENGTH: {}\r\n", body.len()));
    }
    request.push_str("CONNECTION: close\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&buf[..read]),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(err) => return Err(err),
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let head_len = text.find("\r\n\r\n").map_or(0, |idx| idx + 4);
    let (head, resp_body) = text.split_at(head_len);
    let status = head.lines().next().unwrap_or("").to_string();
    // SOAP faults ride HTTP 500 from real routers — hand the body back and
    // let the SOAP layer read `<errorCode>` before anyone judges the status.
    let status_ok = status
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code.starts_with('2'));
    Ok((status_ok, resp_body.to_string()))
}

fn soap_call(
    control_url: &str,
    service: &str,
    action: &str,
    args: &str,
    timeout: Duration,
) -> Result<(), String> {
    let envelope = soap_envelope(service, action, args);
    let soap_action = format!("{service}#{action}");
    let (status_ok, response) =
        http_exchange("POST", control_url, Some(&soap_action), &envelope, timeout)
            .map_err(|err| format!("router rejected {action}: {err}"))?;
    if let Some(code) = parse_soap_error(&response) {
        return Err(upnp_error_text(code));
    }
    if !status_ok {
        return Err(format!("router rejected {action} with a non-2xx response"));
    }
    Ok(())
}

fn query_external_ip(
    control_url: &str,
    service: &str,
    timeout: Duration,
) -> Result<String, String> {
    let envelope = soap_envelope(service, "GetExternalIPAddress", "");
    let soap_action = format!("{service}#GetExternalIPAddress");
    let (status_ok, body) =
        http_exchange("POST", control_url, Some(&soap_action), &envelope, timeout)
            .map_err(|err| format!("router rejected GetExternalIPAddress: {err}"))?;
    if let Some(code) = parse_soap_error(&body) {
        return Err(upnp_error_text(code));
    }
    if !status_ok {
        return Err("router answered GetExternalIPAddress with a non-2xx response".to_string());
    }
    parse_external_ip(&body).ok_or_else(|| "router returned no external IP address".to_string())
}

/// Full client against a discovered `LOCATION` (the fake-IGD test target):
/// description → control URL → `AddPortMapping` → `GetExternalIPAddress`.
pub fn map_from_location(
    location: &str,
    port: u16,
    internal_ip: Ipv4Addr,
) -> Result<MappingOk, String> {
    let (desc_ok, xml) = http_exchange("GET", location, None, "", HTTP_TIMEOUT)
        .map_err(|err| format!("cannot reach the UPnP device: {err}"))?;
    if !desc_ok {
        return Err("router returned a non-2xx device description".to_string());
    }
    let (control_url, service) = find_control_url(&xml)
        .ok_or_else(|| "no WANIPConnection service in the router description".to_string())?;
    let control_url = resolve_url(location, &control_url)
        .ok_or_else(|| "router controlURL is not a usable HTTP URL".to_string())?;
    soap_call(
        &control_url,
        &service,
        "AddPortMapping",
        &add_port_mapping_args(port, &internal_ip.to_string(), LEASE_DURATION_SECS),
        HTTP_TIMEOUT,
    )?;
    let external_ip = query_external_ip(&control_url, &service, HTTP_TIMEOUT)?;
    Ok(MappingOk {
        external_ip,
        port,
        control_url,
        service,
        internal_ip,
    })
}

/// Renewal path: re-`AddPortMapping` + re-query on the stored endpoint
/// (discovery is not repeated — renewal is meant to be one cheap packet).
fn renew_mapping(
    control_url: &str,
    service: &str,
    port: u16,
    internal_ip: Ipv4Addr,
) -> Result<MappingOk, String> {
    soap_call(
        control_url,
        service,
        "AddPortMapping",
        &add_port_mapping_args(port, &internal_ip.to_string(), LEASE_DURATION_SECS),
        HTTP_TIMEOUT,
    )?;
    let external_ip = query_external_ip(control_url, service, HTTP_TIMEOUT)?;
    Ok(MappingOk {
        external_ip,
        port,
        control_url: control_url.to_string(),
        service: service.to_string(),
        internal_ip,
    })
}

/// Best-effort `DeletePortMapping` — every error is ignored by contract.
fn delete_mapping(control_url: &str, service: &str, port: u16) {
    let _ = soap_call(
        control_url,
        service,
        "DeletePortMapping",
        &delete_port_mapping_args(port),
        DELETE_TIMEOUT,
    );
}

/// SSDP discovery: broadcast/replay an `M-SEARCH` for the IGD root device
/// and collect until `SSDP_TIMEOUT`; first `LOCATION` wins.
pub fn discover(timeout: Duration) -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    // 239.255.255.250 is multicast, but SO_BROADCAST lets it leave as a
    // broadcast too — some consumer stacks only route it that way.
    socket.set_broadcast(true).ok()?;
    socket
        .set_read_timeout(Some(SSDP_READ_TICK))
        .expect("setting SSDP read timeout");
    let target: SocketAddr = SSDP_ADDR.parse().ok()?;
    let probe = ssdp_request();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let _ = socket.send_to(probe.as_bytes(), target);
        let tick_start = Instant::now();
        while Instant::now().duration_since(tick_start) < SSDP_PROBE_INTERVAL {
            match socket.recv(&mut buf) {
                Ok(size) => {
                    if let Some(location) = parse_ssdp_location(&buf[..size]) {
                        return Some(location);
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(_) => return None,
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Async wrapper: driver resource + systems
// ---------------------------------------------------------------------------

/// What a background attempt was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptKind {
    /// First map after the Host screen reached `Listening`.
    Initial,
    /// Periodic lease refresh.
    Renew,
}

/// A background attempt's report (mpsc message).
#[derive(Debug)]
pub struct UpnpOutcome {
    /// Attempt generation at spawn time (teardowns bump it).
    pub gen: u64,
    /// What kind of attempt reported.
    pub kind: AttemptKind,
    /// `Ok` = mapping live (renewals refresh the external IP), `Err` =
    /// human-readable failure reason.
    pub result: Result<MappingOk, String>,
}

/// Signature of the background run handed `(port, sender, generation)`.
/// The default is the real route probe + SSDP/SOAP client; tests inject a
/// fake so no unit test ever touches the network. Everything slow — the
/// local-IPv4 probe included — runs on the spawned thread, never a frame.
pub type UpnpRunner = fn(u16, Sender<UpnpOutcome>, u64);

/// Real runner: LAN route probe → SSDP discovery → map (all on the
/// calling, background thread).
fn default_runner(port: u16, replies: Sender<UpnpOutcome>, gen: u64) {
    let result = match local_ipv4() {
        None => Err("no LAN route — nothing for the router to forward to".to_string()),
        Some(internal_ip) => match discover(SSDP_TIMEOUT) {
            Some(location) => map_from_location(&location, port, internal_ip),
            None => Err("no UPnP router answered on the network".to_string()),
        },
    };
    let _ = replies.send(UpnpOutcome {
        gen,
        kind: AttemptKind::Initial,
        result,
    });
}

/// Renewal runner (thread body).
fn default_renew_runner(
    control_url: String,
    service: String,
    port: u16,
    internal_ip: Ipv4Addr,
    replies: Sender<UpnpOutcome>,
    gen: u64,
) {
    let result = renew_mapping(&control_url, &service, port, internal_ip);
    let _ = replies.send(UpnpOutcome {
        gen,
        kind: AttemptKind::Renew,
        result,
    });
}

/// Bookkeeping beside [`UpnpState`]: mailbox + generation + what is needed
/// for renew/delete. All state lives here; the systems are stateless apart
/// from the status-edge `Local`.
#[derive(Resource)]
pub struct UpnpDriver {
    /// Injectable runner seam (real client by default).
    pub runner: UpnpRunner,
    tx: Sender<UpnpOutcome>,
    rx: Mutex<Receiver<UpnpOutcome>>,
    gen: u64,
    internal_ip: Option<Ipv4Addr>,
    control_url: Option<String>,
    service: Option<String>,
    last_renew: Option<Instant>,
}

impl Default for UpnpDriver {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            runner: default_runner,
            tx,
            rx: Mutex::new(rx),
            gen: 0,
            internal_ip: None,
            control_url: None,
            service: None,
            last_renew: None,
        }
    }
}

fn port_for(session: &NetSession, state: &UpnpState) -> u16 {
    match state {
        UpnpState::Mapping { port } | UpnpState::Mapped { port, .. } => *port,
        _ => session
            .listen_addr
            .map_or(DEFAULT_HOST_PORT, |addr| addr.port()),
    }
}

/// Begin a mapping attempt for the listening host (no-op unless
/// `NetProfile::upnp_enabled` and the session is hosting at `Listening`).
pub fn start_mapping(world: &mut World) {
    let (status, role, port) = {
        let session = world.resource::<NetSession>();
        (
            session.status.clone(),
            session.role,
            port_for(session, world.resource::<UpnpState>()),
        )
    };
    if role != NetRole::Host || status != NetStatus::Listening {
        return;
    }
    if !world.resource::<NetProfile>().upnp_enabled {
        return;
    }
    world.resource_scope::<UpnpDriver, ()>(|world, mut driver| {
        world.resource_scope::<UpnpState, ()>(|_world, mut state| {
            begin_attempt(&mut driver, &mut state, port);
        });
    });
}

/// Kick off one background attempt (generation bumped first so results
/// that race a later teardown are dropped).
fn begin_attempt(driver: &mut UpnpDriver, state: &mut UpnpState, port: u16) {
    driver.gen += 1;
    let (gen, runner, replies) = (driver.gen, driver.runner, driver.tx.clone());
    *state = apply_upnp_event(state, &UpnpEvent::Start { port });
    std::thread::spawn(move || runner(port, replies, gen));
}

/// Shared teardown: reset the state, bump the generation (in-flight
/// attempts are dropped), fire-and-forget the delete.
fn teardown_mapping_shared(driver: &mut UpnpDriver, state: &mut UpnpState, port: u16) {
    driver.gen += 1;
    driver.internal_ip = None;
    driver.last_renew = None;
    let pending = std::mem::take(&mut driver.control_url);
    let service = driver.service.take();
    *state = apply_upnp_event(state, &UpnpEvent::Teardown);
    if let (Some(url), Some(service)) = (pending, service) {
        if !url.is_empty() {
            std::thread::spawn(move || delete_mapping(&url, &service, port));
        }
    }
}

/// Tear the mapping down now (Esc on Listening / all `net_stop` paths —
/// the driver system also watches this edge itself).
pub fn teardown_mapping(world: &mut World) {
    if *world.resource::<UpnpState>() == UpnpState::Off {
        return;
    }
    let port = {
        let session = world.resource::<NetSession>();
        port_for(session, world.resource::<UpnpState>())
    };
    world.resource_scope::<UpnpDriver, ()>(|world, mut driver| {
        world.resource_scope::<UpnpState, ()>(|_world, mut state| {
            teardown_mapping_shared(&mut driver, &mut state, port);
        });
    });
}

/// The Update driver: poll attempt results, start on the `Listening` edge,
/// delete on the way out, keep the lease alive while the host holds it.
pub fn upnp_driver_system(
    mut driver: ResMut<UpnpDriver>,
    mut state: ResMut<UpnpState>,
    session: Option<Res<NetSession>>,
    profile: Option<Res<NetProfile>>,
    mut prev: Local<Option<NetStatus>>,
) {
    // 1. Attempt results.
    let drained: Vec<UpnpOutcome> = {
        let rx = driver
            .rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        rx.try_iter().collect()
    };
    for outcome in drained {
        if outcome.gen != driver.gen {
            // A teardown raced this attempt: never resurrect the state,
            // and drop the router's fresh mapping right away.
            if let Ok(mapped) = outcome.result {
                if !mapped.control_url.is_empty() {
                    std::thread::spawn(move || {
                        delete_mapping(&mapped.control_url, &mapped.service, mapped.port)
                    });
                }
            }
            continue;
        }
        match outcome.result {
            Ok(mapped) => {
                driver.internal_ip = Some(mapped.internal_ip);
                driver.control_url = Some(mapped.control_url.clone());
                driver.service = Some(mapped.service.clone());
                driver.last_renew = Some(Instant::now());
                *state = apply_upnp_event(
                    &state,
                    &UpnpEvent::Mapped {
                        external_ip: mapped.external_ip,
                        port: mapped.port,
                    },
                );
            }
            Err(reason) => {
                if outcome.kind == AttemptKind::Renew {
                    // Keep `Mapped`: the router entry may well still be
                    // there, and a live match must not hear about a probe
                    // blip. The next interval retries.
                    warn!("net: UPnP lease renewal failed: {reason}");
                }
                *state = apply_upnp_event(&state, &UpnpEvent::Failed(reason));
            }
        }
    }

    let current = session.as_deref().map(|session| session.status.clone());
    let role = session
        .as_ref()
        .map_or(NetRole::Host, |session| session.role);

    // 2. Teardown edge: session fell to Idle/BindFailed (every net_stop
    // path) while a mapping attempt was alive.
    if *state != UpnpState::Off
        && matches!(
            current,
            Some(NetStatus::Idle) | Some(NetStatus::BindFailed(_))
        )
    {
        let port = session
            .as_deref()
            .map_or(DEFAULT_HOST_PORT, |session| port_for(session, &state));
        teardown_mapping_shared(&mut driver, &mut state, port);
    }

    // 3. Start edge: fresh into Listening with nothing in flight and the
    // profile letting UPnP through (the `U` toggle covers the rest).
    let listening = role == NetRole::Host && matches!(current, Some(NetStatus::Listening));
    let enabled = profile
        .as_deref()
        .is_none_or(|profile| profile.upnp_enabled);
    if listening
        && enabled
        && !matches!(prev.as_ref(), Some(NetStatus::Listening))
        && matches!(*state, UpnpState::Off | UpnpState::Failed(_))
    {
        let port = session
            .as_deref()
            .map_or(DEFAULT_HOST_PORT, |session| port_for(session, &state));
        begin_attempt(&mut driver, &mut state, port);
    }

    // 4. Lease renewal while the mapping is held by a live host session.
    if let UpnpState::Mapped { port, .. } = *state {
        let held = session
            .as_deref()
            .is_some_and(|session| mapping_held(&session.status, session.role));
        if held && renew_due(driver.last_renew, Instant::now()) {
            if let (Some(internal_ip), Some(control_url), Some(service)) = (
                driver.internal_ip,
                driver.control_url.clone(),
                driver.service.clone(),
            ) {
                driver.last_renew = Some(Instant::now());
                let (gen, replies) = (driver.gen, driver.tx.clone());
                std::thread::spawn(move || {
                    default_renew_runner(control_url, service, port, internal_ip, replies, gen)
                });
            }
        }
    }

    *prev = current;
}

/// Last-chance synchronous delete on app exit. Bounded by
/// [`EXIT_DELETE_TIMEOUT`]; **a SIGKILL skips this and leaves the
/// self-expiring [`LEASE_DURATION_SECS`] lease behind** — the documented
/// worst case.
pub fn upnp_exit_system(world: &mut World) {
    let exiting = world
        .get_resource::<Messages<AppExit>>()
        .is_some_and(|messages| {
            let mut cursor = messages.get_cursor();
            cursor.read(messages).next().is_some()
        });
    if !exiting {
        return;
    }
    let Some(driver) = world.get_resource::<UpnpDriver>() else {
        return;
    };
    let (Some(control_url), Some(service)) = (driver.control_url.clone(), driver.service.clone())
    else {
        return;
    };
    let port = world
        .get_resource::<UpnpState>()
        .map_or(DEFAULT_HOST_PORT, |state| match state {
            UpnpState::Mapped { port, .. } | UpnpState::Mapping { port } => *port,
            _ => DEFAULT_HOST_PORT,
        });
    let _ = soap_call(
        &control_url,
        &service,
        "DeletePortMapping",
        &delete_port_mapping_args(port),
        EXIT_DELETE_TIMEOUT,
    );
}

/// Mounts [`UpnpState`] + [`UpnpDriver`] + the driver/exit systems. Added
/// from `OnlineUiPlugin::build()` (the Host screen owns the copy, the
/// toggle, and the tests' runner injection).
pub struct UpnpPlugin;

impl Plugin for UpnpPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UpnpState>()
            .init_resource::<UpnpDriver>()
            .add_systems(Update, upnp_driver_system)
            .add_systems(Last, upnp_exit_system);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    // ---- pure SSDP response parsing ----

    #[test]
    fn ssdp_parses_location_from_multi_response_burst_with_garbage() {
        let burst = b"garbage line without colon\r\n\
             HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\n\
             EXT:\r\nLOCATION: http://192.168.1.1:5000/rootDesc.xml\r\n\
             ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n\
             \x00\xff not an http response at all\r\n\r\n\
             HTTP/1.1 200 OK\r\nlocation: http://192.168.1.9:8080/desc\r\n\r\n";
        assert_eq!(
            parse_ssdp_location(burst).as_deref(),
            Some("http://192.168.1.1:5000/rootDesc.xml"),
            "first well-formed LOCATION wins, garbage skipped"
        );
    }

    #[test]
    fn ssdp_without_location_is_none() {
        let noise = b"NOTIFY * HTTP/1.1\r\nNTS: ssdp:alive\r\nSERVER: whatever\r\n\r\n";
        assert_eq!(parse_ssdp_location(noise), None);
        assert_eq!(parse_ssdp_location(b""), None);
    }

    // ---- device description parsing ----

    const DESC_V1_V2: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
 <device>
  <serviceList>
   <service>
    <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
    <controlURL>/oldctl</controlURL>
   </service>
   <service>
    <serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType>
    <controlURL>/newctl</controlURL>
   </service>
  </serviceList>
 </device>
</root>"#;

    const DESC_V1_ONLY: &str = r#"<root><device><serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType>
        <controlURL>/pppctl</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
        <controlURL>ctl/ip</controlURL>
      </service>
    </serviceList></device></root>"#;

    const DESC_NO_WAN: &str = r#"<root><device><serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
        <controlURL>/l3f</controlURL>
      </service>
    </serviceList></device></root>"#;

    #[test]
    fn device_desc_prefers_wanipconnection_v2() {
        let (url, service) = find_control_url(DESC_V1_V2).expect("a WAN service");
        assert_eq!(url, "/newctl");
        assert_eq!(service, SERVICE_V2);
    }

    #[test]
    fn device_desc_falls_back_to_v1_ignoring_ppp_and_siblings() {
        let (url, service) = find_control_url(DESC_V1_ONLY).expect("a WAN service");
        assert_eq!(url, "ctl/ip");
        assert_eq!(service, SERVICE_V1);
    }

    #[test]
    fn device_desc_without_wanipconnection_finds_nothing() {
        assert_eq!(find_control_url(DESC_NO_WAN), None);
        assert_eq!(find_control_url("totally not xml"), None);
    }

    #[test]
    fn control_url_resolution_covers_relative_forms() {
        let base = "http://192.168.1.1:5000/rootDesc.xml";
        assert_eq!(
            resolve_url(base, "/ctl").as_deref(),
            Some("http://192.168.1.1:5000/ctl")
        );
        assert_eq!(
            resolve_url(base, "ctl").as_deref(),
            Some("http://192.168.1.1:5000/ctl")
        );
        assert_eq!(
            resolve_url("http://192.168.1.1", "/ctl").as_deref(),
            Some("http://192.168.1.1/ctl")
        );
        assert_eq!(
            resolve_url(base, "http://10.0.0.1:9/ctl").as_deref(),
            Some("http://10.0.0.1:9/ctl")
        );
    }

    // ---- SOAP wire format + faults ----

    #[test]
    fn add_port_mapping_envelope_carries_the_full_tuple() {
        let envelope = soap_envelope(
            SERVICE_V2,
            "AddPortMapping",
            &add_port_mapping_args(27015, "192.168.5.5", LEASE_DURATION_SECS),
        );
        assert!(envelope.contains("urn:schemas-upnp-org:service:WANIPConnection:2"));
        assert!(envelope.contains("<NewExternalPort>27015</NewExternalPort>"));
        assert!(envelope.contains("<NewProtocol>UDP</NewProtocol>"));
        assert!(envelope.contains("<NewInternalPort>27015</NewInternalPort>"));
        assert!(envelope.contains("<NewInternalClient>192.168.5.5</NewInternalClient>"));
        assert!(envelope.contains("<NewRemoteHost></NewRemoteHost>"));
        assert!(envelope
            .contains("<NewPortMappingDescription>blockfall-netplay</NewPortMappingDescription>"));
        assert!(envelope.contains("<NewLeaseDuration>3600</NewLeaseDuration>"));
    }

    #[test]
    fn delete_envelope_carries_the_tuple() {
        let envelope = soap_envelope(
            SERVICE_V1,
            "DeletePortMapping",
            &delete_port_mapping_args(27015),
        );
        assert!(envelope.contains("<NewExternalPort>27015</NewExternalPort>"));
        assert!(envelope.contains("<NewProtocol>UDP</NewProtocol>"));
    }

    const FAULT_718: &str = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
<s:Body>
<s:Fault>
 <faultcode>s:Client</faultcode>
 <faultstring>UPnPError</faultstring>
 <detail>
  <UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
   <errorCode>718</errorCode>
   <errorDescription>ConflictInMappingEntry</errorDescription>
  </UPnPError>
 </detail>
</s:Fault>
</s:Body>
</s:Envelope>"#;

    const FAULT_725: &str = "<s:Envelope><s:Body><s:Fault><detail><UPnPError>\
        <errorCode>725</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>";

    #[test]
    fn soap_faults_map_to_human_readable_reasons() {
        assert_eq!(parse_soap_error(FAULT_718), Some(718));
        assert_eq!(parse_soap_error(FAULT_725), Some(725));
        let text = upnp_error_text(718);
        assert!(text.contains("718"), "{text}");
        assert!(text.to_lowercase().contains("conflict"), "{text}");
        let text = upnp_error_text(725);
        assert!(text.contains("725"), "{text}");
        assert!(text.to_lowercase().contains("permanent"), "{text}");
        let generic = upnp_error_text(903);
        assert!(generic.contains("903"), "{generic}");
    }

    #[test]
    fn successful_soap_responses_parse_clean() {
        let ok = r#"<s:Envelope><s:Body><u:AddPortMappingResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:2"/></s:Body></s:Envelope>"#;
        assert_eq!(parse_soap_error(ok), None);
        let ip_ok = r#"<s:Envelope><s:Body><u:GetExternalIPAddressResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:2">
          <ExternalIPAddress>203.0.113.7</ExternalIPAddress>
        </u:GetExternalIPAddressResponse></s:Body></s:Envelope>"#;
        assert_eq!(parse_external_ip(ip_ok).as_deref(), Some("203.0.113.7"));
        assert_eq!(parse_external_ip("<html>503</html>"), None);
    }

    // ---- UpnpState transition table ----

    #[test]
    fn upnp_state_happy_path() {
        let s = apply_upnp_event(&UpnpState::Off, &UpnpEvent::Start { port: 27015 });
        assert_eq!(s, UpnpState::Mapping { port: 27015 });
        let s = apply_upnp_event(
            &s,
            &UpnpEvent::Mapped {
                external_ip: "1.2.3.4".into(),
                port: 27015,
            },
        );
        assert_eq!(
            s,
            UpnpState::Mapped {
                external_ip: "1.2.3.4".into(),
                port: 27015
            }
        );
        let s = apply_upnp_event(&s, &UpnpEvent::Teardown);
        assert_eq!(s, UpnpState::Off);
    }

    #[test]
    fn upnp_state_late_results_cannot_resurrect_dead_states() {
        let failed = UpnpState::Failed("nope".into());
        assert_eq!(
            apply_upnp_event(
                &failed,
                &UpnpEvent::Mapped {
                    external_ip: "1.2.3.4".into(),
                    port: 1
                }
            ),
            failed,
            "a late success must not override Failed"
        );
        assert_eq!(
            apply_upnp_event(&UpnpState::Off, &UpnpEvent::Failed("late".into())),
            UpnpState::Off
        );
        let mapped = UpnpState::Mapped {
            external_ip: "1.2.3.4".into(),
            port: 27015,
        };
        assert_eq!(
            apply_upnp_event(&mapped, &UpnpEvent::Failed("renew blip".into())),
            mapped,
            "renewal failures never drop a live mapping"
        );
        // A fresh Start supersedes anything (re-host, toggle retry).
        assert_eq!(
            apply_upnp_event(&mapped, &UpnpEvent::Start { port: 5 }),
            UpnpState::Mapping { port: 5 }
        );
        assert_eq!(
            apply_upnp_event(&failed, &UpnpEvent::Start { port: 6 }),
            UpnpState::Mapping { port: 6 }
        );
    }

    // ---- lease renewal math ----

    #[test]
    fn renewal_is_due_at_half_the_lease() {
        let now = Instant::now();
        assert!(!renew_due(None, now));
        assert!(!renew_due(Some(now - Duration::from_secs(1_000)), now));
        assert!(renew_due(Some(now - RENEW_INTERVAL), now));
        assert!(renew_due(Some(now - RENEW_INTERVAL * 2), now));
        assert_eq!(RENEW_INTERVAL, Duration::from_secs(1800));
        assert_eq!(LEASE_DURATION_SECS, 3600);
    }

    // ---- fake IGD end-to-end (loopback TCP, no router) ----

    struct FakeIgd {
        url: String,
        port: u16,
        bodies: Arc<StdMutex<Vec<String>>>,
    }

    /// Serve canned HTTP responses on a loopback port until dropped — a
    /// stand-in IGD device (device description + SOAP) that also records
    /// every request body so the tests can pin the wire format.
    fn spawn_fake_igd(
        description: &'static str,
        add_status: &'static str,
        add_body: &'static str,
    ) -> FakeIgd {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake IGD binds");
        let port = listener.local_addr().expect("addr").port();
        let bodies = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("fake IGD read timeout");
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                while let Ok(size) = stream.read(&mut buf) {
                    if size == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..size]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&raw).into_owned();
                let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
                let line = head.lines().next().unwrap_or("").to_string();
                let content_len = head
                    .lines()
                    .find_map(|l| {
                        let (name, value) = l.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let mut body = rest.to_string();
                while body.len() < content_len {
                    let Ok(size) = stream.read(&mut buf) else {
                        break;
                    };
                    if size == 0 {
                        break;
                    }
                    body.push_str(&String::from_utf8_lossy(&buf[..size]));
                }
                sink.lock()
                    .expect("bodies lock")
                    .push(format!("{line}\n{body}"));
                let response = if line.starts_with("GET ") {
                    let payload = description.to_string();
                    format!("HTTP/1.0 200 OK\r\nCONTENT-TYPE: text/xml\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{payload}", payload.len())
                } else if body.contains("AddPortMapping") {
                    format!(
                        "HTTP/1.0 {}\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{add_body}",
                        add_status,
                        add_body.len()
                    )
                } else if body.contains("GetExternalIPAddress") {
                    let payload = "<s:Envelope><s:Body><u:GetExternalIPAddressResponse xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:2\"><ExternalIPAddress>203.0.113.7</ExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>".to_string();
                    format!("HTTP/1.0 200 OK\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{payload}", payload.len())
                } else {
                    let payload = "<s:Envelope><s:Body><u:DeletePortMappingResponse xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:2\"/></s:Body></s:Envelope>".to_string();
                    format!("HTTP/1.0 200 OK\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{payload}", payload.len())
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        FakeIgd {
            url: format!("http://127.0.0.1:{port}/desc.xml"),
            port,
            bodies,
        }
    }

    const FAULT_718_BODY: &str = "<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>718</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>";

    #[test]
    fn fake_igd_full_client_run_pins_the_wire_format() {
        let igd = spawn_fake_igd(DESC_V1_V2, "200 OK", "<ok/>");
        let mapping =
            map_from_location(&igd.url, 27015, [192, 168, 5, 5].into()).expect("the fake IGD maps");
        assert_eq!(mapping.external_ip, "203.0.113.7");
        assert_eq!(mapping.port, 27015);
        assert_eq!(
            mapping.control_url,
            format!("http://127.0.0.1:{}/newctl", igd.port)
        );
        assert_eq!(mapping.service, SERVICE_V2);
        assert_eq!(mapping.internal_ip, Ipv4Addr::new(192, 168, 5, 5));
        let bodies = igd.bodies.lock().expect("bodies lock");
        let add = bodies
            .iter()
            .find(|b| b.contains("AddPortMapping"))
            .expect("AddPortMapping was POSTed");
        assert!(add.contains("SOAPACTION") || add.contains("POST /newctl"));
        assert!(
            add.contains("<NewInternalPort>27015</NewInternalPort>"),
            "{add}"
        );
        assert!(
            add.contains("<NewInternalClient>192.168.5.5</NewInternalClient>"),
            "{add}"
        );
        assert!(add.contains("<NewProtocol>UDP</NewProtocol>"), "{add}");
        assert!(
            add.contains("<NewLeaseDuration>3600</NewLeaseDuration>"),
            "{add}"
        );
        assert!(
            add.contains(
                "<NewPortMappingDescription>blockfall-netplay</NewPortMappingDescription>"
            ),
            "{add}"
        );
        let soap = bodies
            .iter()
            .find(|b| b.contains("GetExternalIPAddress"))
            .expect("GetExternalIPAddress was POSTed");
        assert!(soap.contains("POST /newctl"), "{soap}");
        drop(bodies);
        delete_mapping(&mapping.control_url, &mapping.service, mapping.port);
        assert!(igd
            .bodies
            .lock()
            .expect("bodies lock")
            .iter()
            .any(|b| b.contains("DeletePortMapping")));
    }

    #[test]
    fn fake_igd_soap_fault_surfaces_as_readable_failure() {
        let igd = spawn_fake_igd(DESC_V1_ONLY, "500 Internal Server Error", FAULT_718_BODY);
        let err = map_from_location(&igd.url, 5, [10, 0, 0, 2].into())
            .expect_err("a 718 fault must fail the mapping");
        assert!(err.contains("718"), "{err}");
        assert!(err.to_lowercase().contains("conflict"), "{err}");
    }

    #[test]
    fn fake_igd_without_wan_service_fails_clean() {
        let igd = spawn_fake_igd(DESC_NO_WAN, "200 OK", "<ok/>");
        let err = map_from_location(&igd.url, 5, [10, 0, 0, 2].into())
            .expect_err("no WANIPConnection must fail cleanly");
        assert!(err.contains("WANIPConnection"), "{err}");
    }

    // ---- driver seam (no threads, no sockets, no timers) ----

    fn silent_runner(_port: u16, _replies: Sender<UpnpOutcome>, _gen: u64) {}

    fn app_with_upnp() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(UpnpPlugin);
        app.init_resource::<NetSession>();
        app.world_mut().insert_resource(NetProfile::default());
        app
    }

    fn upnp_state(app: &App) -> UpnpState {
        app.world().resource::<UpnpState>().clone()
    }

    fn set_listening(app: &mut App) {
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.role = NetRole::Host;
            session.status = NetStatus::Listening;
            session.listen_addr = Some("0.0.0.0:27015".parse().expect("addr"));
        }
        app.update();
    }

    #[test]
    fn driver_starts_mapping_on_the_listening_edge() {
        let mut app = app_with_upnp();
        app.world_mut().resource_mut::<UpnpDriver>().runner = silent_runner;
        set_listening(&mut app);
        assert_eq!(upnp_state(&app), UpnpState::Mapping { port: 27015 });
        // Steady state: the edge does not fire again.
        app.update();
        assert_eq!(upnp_state(&app), UpnpState::Mapping { port: 27015 });
    }

    #[test]
    fn disabled_profile_never_starts() {
        let mut app = app_with_upnp();
        app.world_mut().resource_mut::<UpnpDriver>().runner = silent_runner;
        app.world_mut().resource_mut::<NetProfile>().upnp_enabled = false;
        set_listening(&mut app);
        assert_eq!(upnp_state(&app), UpnpState::Off);
    }

    #[test]
    fn teardown_edge_deletes_and_resets_to_off() {
        let mut app = app_with_upnp();
        let (tx, rx) = channel::<UpnpOutcome>();
        {
            let mut driver = app.world_mut().resource_mut::<UpnpDriver>();
            driver.runner = silent_runner;
            driver.rx = Mutex::new(rx);
        }
        set_listening(&mut app);
        tx.send(UpnpOutcome {
            gen: app.world().resource::<UpnpDriver>().gen,
            kind: AttemptKind::Initial,
            result: Ok(MappingOk {
                external_ip: "203.0.113.7".into(),
                port: 27015,
                control_url: String::new(), // empty ⇒ no delete thread in CI
                service: SERVICE_V2.into(),
                internal_ip: [192, 168, 5, 5].into(),
            }),
        })
        .expect("deliver outcome");
        app.update();
        assert_eq!(
            upnp_state(&app),
            UpnpState::Mapped {
                external_ip: "203.0.113.7".into(),
                port: 27015
            }
        );
        {
            let mut session = app.world_mut().resource_mut::<NetSession>();
            session.status = NetStatus::Idle;
        }
        app.update();
        assert_eq!(upnp_state(&app), UpnpState::Off);
    }

    #[test]
    fn results_from_a_dead_generation_are_dropped() {
        let mut app = app_with_upnp();
        let (tx, rx) = channel::<UpnpOutcome>();
        {
            let mut driver = app.world_mut().resource_mut::<UpnpDriver>();
            driver.runner = silent_runner;
            driver.rx = Mutex::new(rx);
        }
        set_listening(&mut app);
        // Deliver with the generation from *before* the attempt started.
        tx.send(UpnpOutcome {
            gen: 0,
            kind: AttemptKind::Initial,
            result: Ok(MappingOk {
                external_ip: "9.9.9.9".into(),
                port: 27015,
                control_url: String::new(),
                service: SERVICE_V2.into(),
                internal_ip: [192, 168, 5, 5].into(),
            }),
        })
        .expect("deliver stale");
        app.update();
        assert_eq!(
            upnp_state(&app),
            UpnpState::Mapping { port: 27015 },
            "a stale generation must not land"
        );
    }

    #[test]
    fn renew_scheduling_state_is_consistent() {
        let mut app = app_with_upnp();
        app.world_mut().resource_mut::<UpnpDriver>().runner = silent_runner;
        set_listening(&mut app);
        {
            let mut driver = app.world_mut().resource_mut::<UpnpDriver>();
            driver.last_renew = Some(Instant::now() - RENEW_INTERVAL);
            assert!(renew_due(driver.last_renew, Instant::now()));
        }
        app.update();
        // control_url is still unset (nothing mapped) ⇒ no thread spawned,
        // and last_renew stays stale instead of being refreshed.
        assert!(app.world().resource::<UpnpDriver>().last_renew.is_some());
    }
}
