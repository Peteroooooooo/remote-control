# Self-Hosting a Relay on Home Broadband

If you don't want to rent a cloud VPS, you can run `rc-relay` on a home Raspberry Pi, Linux SBC, or mini server.

Before skipping a cloud server, check the network limits and setup work involved:

## 1. When It Works (Public IPv4 vs. IPv6-Only at Home)

- **Home broadband has a public IPv4**: Works for all networks (including IPv4-only corporate LANs) once port forwarding/bridge mode is configured.
- **Home broadband only has public IPv6**: Only reachable from networks that also have IPv6. Common symmetric NAT (NAT4) networks vary:
  - **Mobile 4G / 5G hotspots**: Almost always have IPv6 — works.
  - **Campus networks**: Many have IPv6, some do not.
  - **Corporate / office LANs**: Most lack IPv6 entirely, so an IPv6-only home relay won't help there — use a VPS with public IPv4 instead.
- **Public IPv6 on the host**: Connect the host directly to your ISP modem/ONT, or enable IPv6 passthrough / AP mode on your secondary router so it obtains a public IPv6 address.

## 2. Allow Inbound Traffic on the ISP Modem (Pick One)

Most ISP modems in default router mode block unsolicited inbound connections. You must do one of the following:

- **Bridge mode (recommended)**: Switch the ISP modem to bridge mode, dial with your own router, and allow `7843/TCP` and `7842/UDP`.
- **Modem firewall rule**: Log into the modem's admin panel and disable the IPv6 firewall (or forward/allow `7843/TCP` and `7842/UDP` to the relay host).

## 3. Bind a Domain via DDNS (Recommended)

Residential IPs/prefixes change on reconnect. Pair a domain with DDNS and set `RC_RELAY_PUBLIC` so you don't have to re-paste the relay code whenever your IP changes:

```yaml
services:
  rc-relay:
    image: ghcr.io/peteroooooooo/rc-relay:latest
    container_name: rc-relay
    restart: unless-stopped
    network_mode: host
    environment:
      RC_RELAY_PUBLIC: "relay.example.com"
    volumes:
      - rc-relay:/var/lib/rc-relay

volumes:
  rc-relay:
```

See [apps/rc-relay/README.en.md](../apps/rc-relay/README.en.md) for custom TLS certificates and management commands.
