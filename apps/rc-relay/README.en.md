[简体中文](README.md) · **English**

# rc-relay: Self-Hosted Private Relay

A private relay exclusively for your own devices: when two PCs cannot connect directly via P2P, encrypted traffic is forwarded through it. The relay only sees encrypted bytes — never screen video, input, or commands.

---

## Who It Relays For

- **Members**: PCs that have claimed a relay code. Members sharing the same code can relay traffic with each other. One relay can issue multiple codes for different people or groups; traffic is never forwarded across different codes.
- **Guests**: Devices reported by each member PC — its **My Devices**, **Trusted Devices**, and active **Temporary Devices**. A guest can only relay with the specific member PC that brought it in.
- **Everyone else**: Can reach the port, but cannot forward anything.

Removing a device in the TUI takes effect within 1 second. If a guest is kicked, the member PC that brought it in removes it as well.

---

## Deploy (Docker)

1. Create a new Stack in Portainer, 1Panel, Dockge, etc. using [deploy/docker-compose.yml](deploy/docker-compose.yml), or run `docker compose up -d` on your server.
2. Open `7843/TCP` (required) and `7842/UDP` (recommended, helps P2P hole-punching) in your server firewall and cloud security group.
3. Copy the relay code from `docker logs rc-relay` and paste it into **Settings → Private Relays** on each of your PCs.

The `/var/lib/rc-relay` volume stores relay codes, members, and TLS certificates. Back it up when migrating servers; if it is lost, all PCs will need to paste a new relay code.

---

## Addresses & Certificates

Zero configuration is needed by default: the relay detects public IPv4 and IPv6 addresses from network interfaces (falling back to `api.ipify.org` if no public IPv4 is bound locally), generates a self-signed certificate, and embeds both the addresses and certificate fingerprint into the relay code. The client verifies the certificate by fingerprint, so no domain or Let's Encrypt setup is required.

| Scenario | What to Do |
| --- | --- |
| Public IPv4, IPv6, or dual-stack | No configuration needed. On dual-stack, clients try IPv6 first then IPv4; on IPv6-only, clients also need IPv6. |
| Dynamic IP (home broadband, VM without static IP) | Use a domain: set `RC_RELAY_PUBLIC: "relay.example.com"` in compose and point DNS A / AAAA records to the server. |
| Want a standard CA certificate | See below. |

When the relay code embeds raw IPs, **if the server IP changes, every PC must re-paste the code**. When it embeds a domain, simply update the DNS record.

### Using a Standard CA Certificate

Not required: the default self-signed certificate is equally secure. A CA certificate only removes the fingerprint from the URL and lets browsers open it without warnings, at the cost of 90-day renewal.

1. Obtain a certificate for your domain with certbot using HTTP-01 (requires port 80 reachable) or DNS-01 (via DNS provider API, no port 80 needed).
2. Configure a certbot deploy hook to copy the cert and private key to `/etc/rc-relay/tls` after each renewal.
3. Create `/etc/rc-relay/config.toml` based on [deploy/config.example.toml](deploy/config.example.toml):

   ```toml
   state_dir = "/var/lib/rc-relay"
   public_urls = ["https://relay.example.com:7843"]

   [tls]
   cert_mode = "Reloading"      # Re-reads certs every 24h; no restart needed on renewal
   manual_cert_path = "/etc/rc-relay/tls/cert.pem"
   manual_key_path = "/etc/rc-relay/tls/key.pem"
   ```

4. Update compose with the command and volume mounts:

   ```yaml
   services:
     rc-relay:
       image: ghcr.io/peteroooooooo/rc-relay:latest
       container_name: rc-relay
       restart: unless-stopped
       network_mode: host
       command: ["--config", "/etc/rc-relay/config.toml", "serve"]
       volumes:
         - rc-relay:/var/lib/rc-relay
         - /etc/rc-relay/config.toml:/etc/rc-relay/config.toml:ro
         - /etc/rc-relay/tls:/etc/rc-relay/tls:ro

   volumes:
     rc-relay:
   ```

   The container runs as uid `10001` by default and needs read access to the private key. Either `chown` the key in your deploy hook or set `user:` in compose to match the key owner.

### Other Setups

- **Home server behind a router**: Forward `7843/TCP` and `7842/UDP` on your router to the host. If your router's WAN IP is in `100.64.x.x` (CGNAT), IPv4 relaying won't work — use IPv6 instead and allow inbound traffic on the router/ONT firewall.
- **Panel disallows `host` network**: Map `7843:7843/tcp` and `7842:7842/udp` instead, and set `RC_RELAY_PUBLIC` explicitly since the container cannot see the host's public IP. IPv6 usually will not work in bridge mode.
- **Domain on Cloudflare**: Keep the record **DNS only (gray cloud)**. Cloudflare's proxy does not forward port 7843 or UDP and replaces your certificate.

---

## TUI & CLI Commands

Run `ui` in the container console (or `docker exec -it rc-relay ui`) to open the full-screen TUI: view online members and their guests grouped by relay code, with changes taking effect within 1 second. For plain-text output, run `rc-relay codes`.

| Command | Where to Run | What It Does |
| --- | --- | --- |
| `rc-relay serve [--public …]` | Server | Run the relay and print the initial relay code on startup |
| `rc-relay` (or `rc-relay ui`) | Server | Open TUI: `n` new code, `c` copy/show single-line code, `r` rename, `d` remove member or kick guest (with confirmation), `u` undo code/member removal |
| `rc-relay ui --demo` | Any machine | Try the TUI with mock data in a temporary directory (auto-cleaned on exit) |
| `rc-relay codes` | Server | Print all relay codes and their attached devices in plain text |
| `rc-relay claim <relay-code> [--name pc-name] [--key device-key-file]` | Member PC | Join a relay (what "paste relay code" in the desktop app does) |

Relay codes are automatically named `Code 1`, `Code 2`, … (or `码 1`, `码 2`, …) using the lowest available number and can be renamed anytime.

---

## Building Without Docker

```text
cargo build --release -p rc-relay            # Build directly on Windows
```

Build Linux binaries in WSL rather than on a low-memory VPS. Afterwards, check the highest required glibc symbol with
`objdump -T rc-relay | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1` to ensure it does not exceed your server's glibc version.

systemd reference:

- Binary `/opt/rc-relay/rc-relay`, config `/etc/rc-relay/config.toml` ([deploy/config.example.toml](deploy/config.example.toml)), state `/var/lib/rc-relay/`, unit [deploy/rc-relay.service](deploy/rc-relay.service).
- If running the service under a dedicated system user, run the TUI as that same user so state files don't become owned by `root`: `sudo -u rc-relay /opt/rc-relay/rc-relay`.

## Online Self-Check

```text
cargo run --release -p rc-relay --example relay_check -- keygen owner.key   # Generate a test device key
# Run `rc-relay codes` on the relay server to grab a relay code, then claim it with owner.key:
cargo run --release -p rc-relay -- claim <relay-code> --key owner.key --name check
cargo run --release -p rc-relay --example relay_check -- run https://relay.example.com:7843 owner.key
```

The check temporarily replaces and then clears this test member's guest list; remember to remove the test member in the TUI afterwards.

---

## Internals (For Developers)

**State files** (`/var/lib/rc-relay/`):

| File | Contents |
| --- | --- |
| `codes.txt` | Relay code groups |
| `owners.txt` | Members listed under their relay codes |
| `guests.txt` | Guests reported by each member, with device name and category (`My Devices` / `Trusted Devices`) |
| `kicked.txt` | Guests kicked by the relay but not yet acknowledged by their member PC |
| `invites.txt` | Unused connection codes reported by members |
| `status.txt` | Online devices and temporary pairings for the TUI |
| `tls/` | Self-signed TLS certificate |
| `urls.txt` | Public URLs resolved on last startup |

**HTTP API** (shared on the same HTTPS port as `/relay`):

| Endpoint | Caller | Purpose |
| --- | --- | --- |
| `POST /rc/claim` | New member | Join using a relay code |
| `POST /rc/guests` | Member | Replace the full guest list; response includes any guests kicked by the relay |
| `POST /rc/invites` | Member | Replace active connection code tokens and expirations |
| `POST /rc/meet` | New device | Present a connection code token + signature; invalidates token and allows 10 min of mutual relaying with the member until added |
| `POST /rc/watch` | Member | Long-lived stream; sends `sync` immediately on kick, or a blank line every 20s |
| `GET /ping` | Anyone | Health check |

**Device identity**: A device connecting to the relay is identified by its own node ID, or by the device ID in an attached `Authorization: Bearer rcd1…` proof (used for temporary session window IDs). Proof expiration is checked only at connection time.
