FROM rust:1-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates

RUN cargo build --locked --release --bin stitch

FROM debian:bookworm-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --create-home --home-dir /home/stitch --shell /usr/sbin/nologin stitch \
  && mkdir -p /home/stitch/run \
  && chown -R stitch:stitch /home/stitch

COPY --from=builder /src/target/release/stitch /usr/local/bin/stitch
COPY deploy/container-entrypoint.sh /usr/local/bin/stitch-container-entrypoint

RUN chmod 0755 /usr/local/bin/stitch /usr/local/bin/stitch-container-entrypoint

USER stitch
WORKDIR /home/stitch

ENTRYPOINT ["/usr/local/bin/stitch-container-entrypoint"]
CMD ["stitch", "--config", "/home/stitch/run/stitch.toml"]
