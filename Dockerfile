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
# TLS is rustls, so only CA certificates are needed at runtime.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --home-dir /data btdm

COPY --from=build /src/target/release/btdm /usr/local/bin/btdm

USER btdm
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080

# Bind to all interfaces inside the container; the dashboard generates a bearer
# token automatically when it is not on loopback.
ENV BTDM_DATA_DIR=/data
ENTRYPOINT ["btdm", "--data-dir", "/data"]
CMD ["mine"]
