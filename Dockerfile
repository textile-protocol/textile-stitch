FROM rust:1-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release --bin stitch

FROM debian:bookworm-slim

# The uid is pinned, not left to useradd's next-free choice: the bot's config
# directory is bind-mounted from the host, so whoever creates it there — an
# operator, or Stitch — has to know which uid must own it.
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --uid 1000 --user-group --create-home --home-dir /home/stitch --shell /usr/sbin/nologin stitch \
  && mkdir -p /home/stitch/run \
  && chown -R stitch:stitch /home/stitch

# Declares that this binary reserves RFQ capacity against the wallet token, not
# the corridor slug — the panel refuses to put a second pool on a bot whose
# image does not say so, because an older responder would sign a shared token's
# whole balance once per corridor. Production pins STITCH_PANEL_BOT_IMAGE to a
# `sha-*` tag, so the panel cannot infer this from the tag it was given.
LABEL com.textile.stitch.rfq-reservations="token"

COPY --from=builder /src/target/release/stitch /usr/local/bin/stitch
COPY deploy/container-entrypoint.sh /usr/local/bin/stitch-container-entrypoint

RUN chmod 0755 /usr/local/bin/stitch /usr/local/bin/stitch-container-entrypoint

USER stitch
WORKDIR /home/stitch

ENTRYPOINT ["/usr/local/bin/stitch-container-entrypoint"]
CMD ["stitch", "--config", "/home/stitch/run/stitch.toml"]
