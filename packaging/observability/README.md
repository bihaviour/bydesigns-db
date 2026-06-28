# twill-db observability stack

A self-hosted Prometheus + Grafana stack for the `engine-server` metrics
endpoint. Nothing here phones home — Prometheus *pulls* from your own server's
`/metrics`, and Grafana renders a bundled dashboard.

## What you get

- **Prometheus** scraping `engine-server` every 15s (`prometheus.yml`).
- **Grafana** at `http://localhost:3000` (anonymous admin, no login) with the
  **twill-db · engine-server** dashboard auto-provisioned — query throughput,
  error ratio, commits / write-lane activity, the serialized-handoff ratio
  (hot-row contention), page cache hit ratio, WAL throughput, and connection
  churn.

## Use it

1. **Run the server with metrics enabled** (off by default):

   ```bash
   cargo run -p twill-server -- \
     --listen 0.0.0.0:5433 --db file://./srv.db --metrics 0.0.0.0:9100
   # or the management CLI's serve:
   #   twilldb serve file://./srv.db --listen 0.0.0.0:5433   (metrics flag forthcoming)
   ```

   Confirm the endpoint:

   ```bash
   curl -s http://localhost:9100/metrics | head
   curl -s http://localhost:9100/healthz      # -> ok
   ```

2. **Start the stack** (from this directory):

   ```bash
   docker compose up        # add -d to detach
   ```

3. **Open Grafana:** <http://localhost:3000> → the *twill-db · engine-server*
   dashboard. Prometheus is at <http://localhost:9090> (check
   *Status → Targets* shows `twilldb` UP).

## Pointing at a different server

`prometheus.yml` targets `host.docker.internal:9100` — the Docker host, where
`engine-server` runs in the quick-start. To scrape a server elsewhere, edit the
`targets` list (e.g. `10.0.0.5:9100`) and `docker compose restart prometheus`.
On Linux the compose file already maps `host.docker.internal` to the host
gateway; on Docker Desktop it resolves natively.

## Exported metrics (reference)

Wire-level (from the listener):

| Metric | Type | Meaning |
| --- | --- | --- |
| `twilldb_connections_total` | counter | client connections accepted |
| `twilldb_connections_active` | gauge | currently open connections |
| `twilldb_queries_total` | counter | statements executed (ok + error) |
| `twilldb_query_errors_total` | counter | statements that errored |
| `twilldb_rows_returned_total` | counter | result rows returned |
| `twilldb_uptime_seconds` | gauge | exporter uptime |

Engine / storage (read live through the engine):

| Metric | Type | Meaning |
| --- | --- | --- |
| `twilldb_engine_commits_total` | counter | durable transaction commits |
| `twilldb_engine_durable_appends_total` | counter | group-commit WAL batches |
| `twilldb_engine_committed_lsn` | gauge | highest committed (visible) LSN |
| `twilldb_engine_write_acquires_total` | counter | write-lane acquisitions |
| `twilldb_engine_write_handoffs_total` | counter | acquisitions that waited (handoff) |
| `twilldb_engine_write_wait_us_total` | counter | µs blocked on the write lane |
| `twilldb_storage_wal_appends_total` | counter | durable WAL appends at the seam |
| `twilldb_storage_wal_bytes_total` | counter | WAL bytes appended |
| `twilldb_storage_page_reads_total` | counter | page versions read |
| `twilldb_storage_cache_hits_total` | counter | page reads served warm |
| `twilldb_storage_cache_misses_total` | counter | page reads that hit the backend |
| `twilldb_storage_fetch_latency_us_total` | counter | µs of backend fetch latency |
| `twilldb_storage_fsyncs_total` | counter | fsync / durable-flush ops |

The same engine gauges are also available in-band over SQL via
`SHOW twill.stats` if you'd rather pull them through a Postgres client.
