# rc-relay in a container. Build from the workspace root (cargo needs every
# member of the workspace):
#
#   docker build -f apps/rc-relay/Dockerfile -t rc-relay .
#
# Run with the host's network, so the relay sees the server's own IPv4 and
# IPv6 addresses (see deploy/docker-compose.yml and README.md).

FROM rust:1.94-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p rc-relay \
    && cp target/release/rc-relay /rc-relay

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /var/lib/rc-relay rc-relay \
    && mkdir -p /var/lib/rc-relay \
    && chown rc-relay /var/lib/rc-relay \
    && ln -s /usr/local/bin/rc-relay /usr/local/bin/ui
COPY --from=build /rc-relay /usr/local/bin/rc-relay
ENV LANG=C.UTF-8
USER rc-relay
# Owners, guest lists, claim codes and the relay's own certificate.
VOLUME /var/lib/rc-relay
# HTTPS (relay, claim, guest lists) and QUIC address discovery.
EXPOSE 7843/tcp 7842/udp
ENTRYPOINT ["rc-relay"]
CMD ["serve"]
