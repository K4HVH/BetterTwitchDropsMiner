# Build with the same toolchain the workspace targets.
FROM rust:1.96-slim AS build

# rusqlite is vendored (the `bundled` feature) so it needs a C toolchain, but
# nothing from it ends up as a runtime dependency.
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p btdm

FROM debian:trixie-slim
# TLS is rustls, so only CA certificates are needed at runtime. `curl` is here
# for the healthcheck below and nothing else.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --home-dir /data btdm

COPY --from=build /src/target/release/btdm /usr/local/bin/btdm

USER btdm
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080

# The probe reports whether mining is actually earning, not merely whether the
# process is alive — a miner whose token expired an hour ago is "up" and
# useless. It returns 503 when any account has stalled, so an orchestrator
# restarts it. Deliberately unauthenticated and deliberately free of account
# names, channels or campaigns.
HEALTHCHECK --interval=2m --timeout=5s --start-period=90s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/api/health || exit 1

# Bind to all interfaces inside the container; the dashboard generates a bearer
# token automatically when it is not on loopback.
ENV BTDM_DATA_DIR=/data
ENTRYPOINT ["btdm", "--data-dir", "/data"]
CMD ["mine"]
