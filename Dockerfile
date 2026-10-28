# syntax=docker/dockerfile:1
# The tokenrein binary on a distroless, non-root runtime image.
#   podman build -t tokenrein:dev .     (or docker build)
# On a builder with little memory (e.g. a 4 GB podman VM), cap parallel compiles: DuckDB's C++
# sources take about 1 GB each.   podman build --build-arg JOBS=2 -t tokenrein:dev .

ARG RUST_VERSION=1.96

FROM docker.io/library/rust:${RUST_VERSION}-bookworm AS build
ARG JOBS
WORKDIR /src
# Build the dependencies first so source edits do not rebuild them.
COPY Cargo.toml Cargo.lock ./
COPY proxy/Cargo.toml proxy/Cargo.toml
RUN mkdir -p proxy/src \
    && echo 'fn main() {}' > proxy/src/main.rs \
    && touch proxy/src/lib.rs \
    && cargo build --release --locked ${JOBS:+-j $JOBS} -p tokenrein \
    && rm -rf proxy/src target/release/deps/tokenrein-* target/release/deps/libtokenrein-* \
       target/release/.fingerprint/tokenrein-*
COPY proxy proxy
# The history folder gets a placeholder: Docker and Podman copy a mount point's content (and
# with it, its owner) into a new volume only when there is some, else the volume stays root's.
RUN cargo build --release --locked ${JOBS:+-j $JOBS} -p tokenrein \
    && install -D target/release/tokenrein /out/tokenrein \
    && mkdir -p /out/history \
    && touch /out/history/.keep

FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.title="tokenrein" \
      org.opencontainers.image.description="Runtime spend guardrails for AI agents" \
      org.opencontainers.image.source="https://github.com/geektuhin123/tokenrein" \
      org.opencontainers.image.licenses="Elastic-2.0"
COPY --from=build /out/tokenrein /usr/local/bin/tokenrein
# Usage history (Parquet files): mount a volume here and set TOKENREIN_HISTORY_DIR to it.
COPY --from=build --chown=65532:65532 /out/history /var/lib/tokenrein/history
COPY deploy/tokenrein.toml /etc/tokenrein/tokenrein.toml
ENV TOKENREIN_CONFIG=/etc/tokenrein/tokenrein.toml \
    RUST_LOG=info
# 8787: the proxy (agents and SDKs). 8788: admin API, dashboard, /metrics, /readyz.
EXPOSE 8787 8788
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/tokenrein"]
