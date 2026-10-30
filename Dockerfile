# syntax=docker/dockerfile:1
#
# Container image for the headless gateway (`lanius`, crate `lanius-cli`).
#
#   docker build -t lanius .
#   docker run --rm -p 18000:18000 \
#     -e PROXY_API_KEY=... -e KIRO_REGION=us-east-1 -e KIRO_API_KEY=ksk_... lanius
#
# Multi-arch: docker buildx build --platform linux/amd64,linux/arm64 -t lanius .
# See docs/container.md for Kubernetes notes.

FROM rust:1-alpine AS builder
# gcc/musl-dev: C toolchain for aws-lc-sys (rustls) and bundled SQLite (rusqlite).
RUN apk add --no-cache gcc musl-dev
WORKDIR /src
COPY . .
# The musl target links statically by default, so the binary runs on a
# distroless/static base with no libc.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p lanius-cli \
    && cp target/release/lanius /lanius

# distroless/static provides CA certificates (needed for HTTPS to Kiro), tzdata,
# and a non-root user (65532), with no shell or package manager.
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /lanius /usr/local/bin/lanius
# Writable home of the nonroot user; DEBUG_DIR (default `debug_logs`) is
# resolved relative to it.
WORKDIR /home/nonroot
USER 65532:65532
ENV SERVER_HOST=0.0.0.0 \
    SERVER_PORT=18000
EXPOSE 18000
ENTRYPOINT ["/usr/local/bin/lanius"]
