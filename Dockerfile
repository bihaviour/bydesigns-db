# engine-server (Phase 3): the twill-db engine behind a Postgres-wire listener.
# Multi-stage: build the release binary, then ship it on a slim runtime.
#
#   docker build -t twill-db .
#   docker run -p 5433:5433 -v twilldata:/data twill-db \
#     --listen 0.0.0.0:5433 --db file:///data/srv.db
#
# Any Postgres client connects (cleartext): sslmode=disable.

FROM rust:1.80-bookworm AS build
WORKDIR /src
# Copy the whole workspace; the engine hand-rolls its deps so builds are cheap.
COPY . .
RUN cargo build -p twill-server --release \
    && cp target/release/engine-server /usr/local/bin/engine-server

FROM debian:bookworm-slim AS runtime
# Durable data lives under /data; mount a volume here to persist across restarts.
RUN useradd --system --uid 10001 --create-home --home-dir /data twill \
    && mkdir -p /data && chown twill:twill /data
USER twill
WORKDIR /data
EXPOSE 5433
COPY --from=build /usr/local/bin/engine-server /usr/local/bin/engine-server
# Bind to all interfaces inside the container and persist to the data volume by
# default; override either by appending flags to `docker run … <image> --flag`.
ENTRYPOINT ["engine-server"]
CMD ["--listen", "0.0.0.0:5433", "--db", "file:///data/srv.db"]
