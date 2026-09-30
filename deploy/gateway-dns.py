#!/usr/bin/env python3
"""Keep the blockfall DNS A record pointed at this gateway's egress IP.

Runs as a systemd oneshot (see blockfall-dns.timer). DynamicDNS semantics:
resolve our public egress IP, compare against the DO-managed A record, update
on drift. Also refuses to advertise a dead gateway: a local UDP liveness
probe (`*G` -> `*E` on the control port) gates every write, so if the gateway
process is down the record stays as it is and a future cloud standby can own
it cleanly.

Config via EnvironmentFile (default /etc/blockfall-dns.env):
  DO_DNS_TOKEN   DigitalOcean PAT, scope dns:read + dns:write ONLY  (required)
  DNS_DOMAIN     default opensensor.io
  DNS_RECORD     default blockfall
  TETRIS_PORT    gateway control port, default 27016
  IP_SOURCES     comma list, default https://api.ipify.org,https://ipv4.icanhazip.com
  DNS_TTL        default 60

Flags: --once (default) --dry-run (print decisions, write nothing)
Exit non-zero on any failure; the 60 s timer retries. stdlib only.
"""

import argparse
import json
import os
import socket
import sys
import urllib.request

API = "https://api.digitalocean.com/v2"


def env(key, default=None):
    return os.environ.get(key, default)


def egress_ip(sources):
    last = None
    for url in sources:
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "blockfall-dns"})
            with urllib.request.urlopen(req, timeout=10) as resp:
                ip = resp.read().decode().strip()
            socket.inet_aton(ip)
            return ip
        except Exception as exc:  # noqa: BLE001 - try the next source
            last = exc
    raise RuntimeError(f"no IP source reachable: {last}")


def live_probe(port, timeout=2.0):
    """Gateway liveness: *G on an unlikely code must be answered with *E."""
    probe = b"*GZZZZZ"
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(timeout)
    try:
        sock.sendto(probe, ("127.0.0.1", port))
        data, _ = sock.recvfrom(64)
        return data == b"*EZZZZZ"
    except OSError:
        return False
    finally:
        sock.close()


def api_request(token, method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        f"{API}{path}",
        data=data,
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        raw = resp.read()
        return json.loads(raw) if raw else {}


def find_record(token, domain, name):
    page = 1
    while True:
        doc = api_request(token, "GET", f"/domains/{domain}/records?type=A&per_page=200&page={page}")
        for rec in doc.get("links", {}) and doc.get("records", []):
            if rec["name"] == name:
                return rec
        if doc.get("meta", {}).get("total", 0) <= page * 200:
            return None
        page += 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--once", action="store_true", help="single pass (default)")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    token = env("DO_DNS_TOKEN")
    if not token:
        print("DO_DNS_TOKEN unset — nothing to do", file=sys.stderr)
        return 1
    domain = env("DNS_DOMAIN", "opensensor.io")
    name = env("DNS_RECORD", "blockfall")
    port = int(env("TETRIS_PORT", "27016"))
    ttl = int(env("DNS_TTL", "60"))
    sources = env("IP_SOURCES", "https://api.ipify.org,https://ipv4.icanhazip.com").split(",")

    if not live_probe(port):
        print(f"gateway not answering on 127.0.0.1:{port} — leaving DNS alone", file=sys.stderr)
        return 1

    ip = egress_ip(sources)
    rec = find_record(token, domain, name)

    if rec is None:
        print(f"CREATE A {name}.{domain} -> {ip} (ttl {ttl})")
        if not args.dry_run:
            api_request(token, "POST", f"/domains/{domain}/records",
                        {"type": "A", "name": name, "data": ip, "ttl": ttl})
    elif rec["data"] != ip:
        print(f"UPDATE A {name}.{domain}: {rec['data']} -> {ip}")
        if not args.dry_run:
            api_request(token, "PUT", f"/domains/{domain}/records/{rec['id']}",
                        {"data": ip, "ttl": ttl})
    else:
        print(f"current: A {name}.{domain} -> {ip}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
