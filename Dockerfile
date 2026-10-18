# syntax=docker/dockerfile:1.7
ARG PG_MAJOR=18

FROM postgres:${PG_MAJOR} AS build
ARG PG_MAJOR
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl pkg-config libssl-dev clang libclang-dev \
      postgresql-server-dev-${PG_MAJOR} \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
RUN cargo install cargo-pgrx --version 0.19.3 --locked
RUN cargo pgrx init --pg${PG_MAJOR} /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config
# pg_locale.h includes ICU headers, which bindgen needs to parse the server headers.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libicu-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock pg_kv.control ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo pgrx package \
      --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config \
      --no-default-features --features pg${PG_MAJOR} \
 && mkdir -p /out \
 && cp -r target/release/pg_kv-pg${PG_MAJOR}/* /out/

FROM postgres:${PG_MAJOR}
COPY --from=build /out/ /
CMD ["postgres", "-c", "shared_preload_libraries=pg_kv"]
