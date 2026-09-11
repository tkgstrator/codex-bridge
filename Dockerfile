# syntax=docker/dockerfile:1
# Multi-arch: rust:alpine builds a fully static musl binary natively on
# each platform, so `docker buildx build --platform linux/amd64,linux/arm64`
# needs no cross-compilation setup.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /app

# Cache dependency compilation separately from the source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/codex-bridge target/release/deps/codex_bridge-*

COPY src ./src
RUN cargo build --release --locked

# distroless/static ships CA certs, tzdata and a nonroot user — nothing
# else. The binary is static (musl + rustls) so no libc is needed.
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /app/target/release/codex-bridge /codex-bridge
ENV PORT=3000
ENV CODEX_AUTH_PATH=/data/auth.json
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["/codex-bridge", "--health"]
ENTRYPOINT ["/codex-bridge"]
