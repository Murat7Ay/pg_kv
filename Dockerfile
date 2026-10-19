# syntax=docker/dockerfile:1.7
#   docker build --build-arg PG_MAJOR=17 -t pg_kv:17 .
ARG PG_MAJOR=18
ARG DEBIAN=trixie

# Rust and cargo-pgrx do not depend on the PostgreSQL major, so every major
# shares this stage (and its cache).
FROM debian:${DEBIAN}-slim AS toolchain
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl pkg-config libssl-dev \
      clang libclang-dev libicu-dev \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
RUN cargo install cargo-pgrx --version 0.19.3 --locked

FROM toolchain AS build
ARG PG_MAJOR
ARG DEBIAN
# Server headers for this major from the PostgreSQL apt repository (PGDG), the
# same packages the official postgres images are built from.
RUN install -d /usr/share/postgresql-common/pgdg \
 && curl -fsSL -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc \
      https://www.postgresql.org/media/keys/ACCC4CF8.asc \
 && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt ${DEBIAN}-pgdg main" \
      > /etc/apt/sources.list.d/pgdg.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends postgresql-server-dev-${PG_MAJOR} \
 && rm -rf /var/lib/apt/lists/*
RUN cargo pgrx init --pg${PG_MAJOR} /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config

WORKDIR /src
COPY Cargo.toml Cargo.lock pg_kv.control ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=pg_kv-target-${PG_MAJOR} \
    cargo pgrx package \
      --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config \
      --no-default-features --features pg${PG_MAJOR} \
 && mkdir -p /out \
 && cp -r target/release/pg_kv-pg${PG_MAJOR}/* /out/

FROM postgres:${PG_MAJOR}-${DEBIAN}
LABEL org.opencontainers.image.source="https://github.com/Murat7Ay/pg_kv" \
      org.opencontainers.image.description="PostgreSQL with pg_kv: a Redis-style cache, rate limiter and sorted sets in shared memory" \
      org.opencontainers.image.licenses="PostgreSQL"
COPY --from=build /out/ /
# Preload pg_kv in every cluster this image initializes, so it still works when
# the command is overridden (Testcontainers, Kubernetes, compose files). This is
# how the official image sets listen_addresses.
RUN sed -ri "s/^#?(shared_preload_libraries)\s*=\s*'[^']*'/\1 = 'pg_kv'/" /usr/share/postgresql/postgresql.conf.sample \
 && grep -q "^shared_preload_libraries = 'pg_kv'" /usr/share/postgresql/postgresql.conf.sample
