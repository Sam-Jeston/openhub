# syntax=docker/dockerfile:1

FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY migrations migrations
COPY src src
COPY static static
COPY templates templates
RUN cargo build --release --locked

FROM debian:bookworm-slim
# git serves fetches and pushes; everything else is in the binary.
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home-dir /data --shell /usr/sbin/nologin openhub \
 && mkdir /data && chown openhub:openhub /data
COPY --from=build /src/target/release/openhub /usr/local/bin/openhub
USER openhub
ENV OPENHUB_DATA_DIR=/data \
    OPENHUB_HTTP_ADDR=0.0.0.0:3000 \
    OPENHUB_SSH_ADDR=0.0.0.0:2222
VOLUME /data
EXPOSE 3000 2222
ENTRYPOINT ["openhub"]
