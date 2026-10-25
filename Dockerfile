# syntax=docker/dockerfile:1
# The tokenlens-proxy binary on a distroless, non-root runtime image.
#   podman build -t tokenlens:dev .     (or docker build)

ARG RUST_VERSION=1.96

FROM docker.io/library/rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
# Build the dependencies first so source edits do not rebuild them.
COPY Cargo.toml Cargo.lock ./
COPY proxy/Cargo.toml proxy/Cargo.toml
RUN mkdir -p proxy/src \
    && echo 'fn main() {}' > proxy/src/main.rs \
    && touch proxy/src/lib.rs \
    && cargo build --release --locked -p tokenlens-proxy \
    && rm -rf proxy/src target/release/deps/tokenlens_proxy-* target/release/deps/libtokenlens_proxy-* \
       target/release/.fingerprint/tokenlens-proxy-*
COPY proxy proxy
RUN cargo build --release --locked -p tokenlens-proxy \
    && install -D target/release/tokenlens-proxy /out/tokenlens-proxy

FROM gcr.io/distroless/cc-debian12:nonroot
LABEL org.opencontainers.image.title="tokenlens-proxy" \
      org.opencontainers.image.description="Runtime spend guardrails for AI agents" \
      org.opencontainers.image.source="https://github.com/geektuhin123/tokenlens" \
      org.opencontainers.image.licenses="Apache-2.0"
COPY --from=build /out/tokenlens-proxy /usr/local/bin/tokenlens-proxy
COPY deploy/tokenlens.toml /etc/tokenlens/tokenlens.toml
ENV TL_CONFIG=/etc/tokenlens/tokenlens.toml \
    RUST_LOG=info
# 8787: the proxy (agents and SDKs). 8788: admin API, dashboard, /metrics, /readyz.
EXPOSE 8787 8788
USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/tokenlens-proxy"]
