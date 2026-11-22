[简体中文](README.md) · **English**

# rc-relay

Private relay server for Remote Control, forwarding end-to-end encrypted traffic exclusively for your devices:

- **Members**: PCs that have claimed the same relay code can relay traffic with each other (multiple isolated code groups are supported).
- **Guests**: A member PC's reported **My Devices**, **Trusted Devices**, and active **Temporary Devices**, allowed to relay only with that member PC.

---

## Docker Deployment

Using [deploy/docker-compose.yml](deploy/docker-compose.yml):

```yaml
services:
  rc-relay:
    image: ghcr.io/peteroooooooo/rc-relay:latest
    container_name: rc-relay
    restart: unless-stopped
    network_mode: host
    volumes:
      - rc-relay:/var/lib/rc-relay

volumes:
  rc-relay:
```

1. Open `7843/TCP` (relay & control) and `7842/UDP` (hole-punch assist).
2. Copy the relay code from `docker logs rc-relay` or `docker exec -it rc-relay ui` and paste it into **Settings → Private Relays** on each client.

Persistent state (relay codes, members, self-signed certificate) is stored in `/var/lib/rc-relay`.

---

## Addresses, Domains & TLS

- **Default (Static Public IPv4 / IPv6)**: Zero config. Automatically detects public IPs, generates a self-signed certificate, and encodes the address and certificate fingerprint into the relay code (re-paste the code if the server IP changes).
- **Dynamic IP / Custom Domain**: Set environment variable `RC_RELAY_PUBLIC: "relay.example.com"` (use DNS-only gray cloud if on Cloudflare).
- **Non-`host` Network Mode**: Map `7843:7843/tcp` and `7842:7842/udp`, and set `RC_RELAY_PUBLIC` to your public IP or domain.
- **Custom TLS Certificate**: Mount `/etc/rc-relay/config.toml` (see [deploy/config.example.toml](deploy/config.example.toml)) and your certificate directory (container runs as `uid 10001` by default and needs read access to the private key):

  ```toml
  state_dir = "/var/lib/rc-relay"
  public_urls = ["https://relay.example.com:7843"]

  [tls]
  cert_mode = "Reloading"      # Periodically reloads certs without restart
  manual_cert_path = "/etc/rc-relay/tls/cert.pem"
  manual_key_path = "/etc/rc-relay/tls/key.pem"
  ```

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

---

## TUI & CLI

Run `ui` inside the container (or `docker exec -it rc-relay ui` on the host) to open the terminal UI. Removing a member or kicking a guest takes effect within 1 second.

| Command | Description |
| --- | --- |
| `rc-relay serve [--public …]` | Start the relay server and print the initial relay code |
| `rc-relay` / `rc-relay ui` | Open TUI (`n` new code, `c` copy code, `r` rename, `d` delete/kick, `u` undo) |
| `rc-relay ui --demo` | Preview the TUI with mock data |
| `rc-relay codes` | Print all relay codes and attached devices in plain text |
| `rc-relay claim <code > [--name name] [--key key-file]` | Claim a relay code from the CLI |

---

## Building from Source & Self-Check

**Build & systemd:**

```bash
cargo build --release -p rc-relay
```

See [deploy/rc-relay.service](deploy/rc-relay.service) and [deploy/config.example.toml](deploy/config.example.toml) for systemd deployment.

**Connectivity self-check:**

```bash
cargo run --release -p rc-relay --example relay_check -- keygen owner.key
cargo run --release -p rc-relay -- claim <relay-code> --key owner.key --name check
cargo run --release -p rc-relay --example relay_check -- run https://relay.example.com:7843 owner.key
```

---

## Internals

**State files (`/var/lib/rc-relay/`):**

| File | Contents |
| --- | --- |
| `codes.txt` | Relay code groups |
| `owners.txt` | Member devices per group |
| `guests.txt` | Guests reported by members (with name and category) |
| `kicked.txt` | Kicked guests pending removal on member PCs |
| `invites.txt` | Active unused connection code tokens |
| `status.txt` | Online status snapshot for the TUI |
| `tls/` | Self-signed certificate and private key |
| `urls.txt` | Public URLs resolved on last startup |

**HTTP API (shares the same HTTPS port as `/relay`):**

| Endpoint | Caller | Purpose |
| --- | --- | --- |
| `POST /rc/claim` | New member | Register as a member using a relay code |
| `POST /rc/guests` | Member | Sync full guest list; returns any guests kicked by the relay |
| `POST /rc/invites` | Member | Sync active connection code tokens and expirations |
| `POST /rc/meet` | New device | Verify and consume a connection code token, authorizing 10 min of temporary relaying with the member |
| `POST /rc/watch` | Member | Long-polling stream: pushes `sync` immediately on kick, or heartbeat every 20s |
| `GET /ping` | Anyone | Health check |
