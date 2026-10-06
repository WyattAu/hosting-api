# hosting-api

Control-plane API for the [WyattAu hosting platform](https://github.com/WyattAu/SimpleInfrastructureStack):
tenant state, backup triggers, and Prometheus metrics over the on-disk tenant
layout that `ops/tenant-provision.sh` produces.

Part of the WyattAu estate — **dogfoods the estate's own crates**:

| Crate | Role here |
| --- | --- |
| [`telemetry-init`](https://crates.io/crates/telemetry-init) | one-call logs/metrics bootstrap |
| [`metrics-kit`](https://crates.io/crates/metrics-kit) | lock-free Prometheus exposition |
| axum + tokio | HTTP + async runtime |

## Deployment

Runs on `docker01` (the tenant Docker host, see
`.docs/hosting-architecture.md` in SIS). Binds `127.0.0.1:8484` by design —
**this API has no authentication of its own**; the edge proxy in front of it
must authenticate before exposing it. Exposing it directly is a
misconfiguration.

```bash
TENANT_ROOT=/srv/tenants HOSTING_LISTEN=127.0.0.1:8484 hosting-api
```

## API

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/healthz` | liveness |
| GET | `/metrics` | Prometheus text 0.0.4 |
| GET | `/api/tenants` | all tenants with live compose status |
| GET | `/api/tenants/{tenant}` | one tenant |
| POST | `/api/tenants/{tenant}/backup?offsite=true` | run `tenant-backup.sh` (mutually excluded per tenant, 15 min budget) |

Credentials metadata is parsed from each tenant's `.credentials` but secrets
are **structurally excluded** from the API's types — they cannot leak because
no field exists to carry them (enforced by tests).

## Development

```bash
cargo build
cargo clippy --all-targets   # estate tier-a posture: pedantic, no panic/unwrap/expect in prod code
cargo test
```

## Known skew (tracked in the SIS engineering loop)

- Published `telemetry-init` 0.1.1 depends on `metrics-kit` 0.1; GitHub main
  has moved to 0.2 but is not republished. This repo pins `metrics-kit = "0.1"`
  to match what crates.io can actually resolve.
- Published `testkit` 0.2.2 lacks `http::TestServer` (main has it); a local
  10-line equivalent lives in the integration tests.

## License

Apache-2.0
