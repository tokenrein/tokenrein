# syntax=docker/dockerfile:1
# The tokenrein binary on a distroless, non-root runtime image.
#   podman build -t tokenrein:dev .     (or docker build)

ARG RUST_VERSION=1.96

FROM docker.io/library/rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
# Build the dependencies first so source edits do not rebuild them.
COPY Cargo.toml Cargo.lock ./
COPY proxy/Cargo.toml proxy/Cargo.toml
RUN mkdir -p proxy/src \
    && echo 'fn main() {}' > proxy/src/main.rs \
    && touch proxy/src/lib.rs \
    && cargo build --release --locked -p tokenrein \
    && rm -rf proxy/src target/release/deps/tokenrein-* target/release/deps/libtokenrein-* \
       target/release/.fingerprint/tokenrein-*
COPY proxy proxy
RUN cargo build --release --locked -p tokenrein \
    && install -D target/release/tokenrein /out/tokenrein

FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.title="tokenrein" \
      org.opencontainers.image.description="Runtime spend guardrails for AI agents" \
      org.opencontainers.image.source="https://github.com/geektuhin123/tokenrein" \
      org.opencontainers.image.licenses="Apache-2.0"
COPY --from=build /out/tokenrein /usr/local/bin/tokenrein
COPY deploy/tokenrein.toml /etc/tokenrein/tokenrein.toml
ENV TOKENREIN_CONFIG=/etc/tokenrein/tokenrein.toml \
    RUST_LOG=info
# 8787: the proxy (agents and SDKs). 8788: admin API, dashboard, /metrics, /readyz.
EXPOSE 8787 8788
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/tokenrein"]
