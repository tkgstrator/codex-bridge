# syntax=docker/dockerfile:1
# Multi-arch: rust:alpine builds a fully static musl binary natively on
# each platform, so `docker buildx build --platform linux/amd64,linux/arm64`
# needs no cross-compilation setup.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /app

# Cache dependency compilation separately from the source. Stub every
# target Cargo.toml declares (the lib, and both binaries — codex-mcp is
# picked up by src/bin/*.rs convention) so this build step compiles the
# same dependency graph the real one does.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && echo 'fn main() {}' > src/bin/codex-mcp.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/codex-bridge target/release/codex-mcp target/release/deps/codex_bridge-* target/release/deps/codex_mcp-*

COPY src ./src
# `COPY` preserves the host's checkout mtimes, which can predate the
# dummy build above — leaving Cargo's fingerprint convinced the real
# lib.rs is the stale stub it already compiled. Force every source file
# newer than that build so it's always seen as changed.
RUN find src -exec touch {} + \
 && cargo build --release --locked

# distroless/static ships CA certs, tzdata and a nonroot user — nothing
# else. The binary is static (musl + rustls) so no libc is needed.
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /app/target/release/codex-bridge /codex-bridge
COPY --from=build /app/target/release/codex-mcp /codex-mcp
ENV PORT=3000
ENV CODEX_AUTH_PATH=/data/auth.json
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["/codex-bridge", "--health"]
# Default entrypoint is the codex-bridge HTTP proxy. To run codex-mcp's
# Streamable HTTP transport instead (e.g. for Claude Code Desktop), override
# it: `docker run --entrypoint /codex-mcp -e MCP_HTTP_PORT=... -e MCP_API_KEY=... ...`
ENTRYPOINT ["/codex-bridge"]
