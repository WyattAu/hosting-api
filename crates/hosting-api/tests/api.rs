//! Integration tests: full router against a fixture tenant root, served
// Test setup may panic on fixture failure; that is the contract for tests.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
// The fixture uses process env to signal stub behaviour to compose stubs.
//! by `testkit::TestServer` (dogfood).
//!
//! The fixture mirrors what `ops/tenant-provision.sh` creates on disk:
//! `compose.sh` (an executable stub) and `.credentials`.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use hosting_api::AppState;
use metrics_kit::Registry;

/// Minimal axum test server on a random port.
///
/// Note: testkit's GitHub main has `http::TestServer`, but the published
/// 0.2.2 does not — republishing is tracked in the engineering loop. This
/// local copy keeps the repo on crates.io-only dependencies.
struct TestServer {
    addr: std::net::SocketAddr,
    _handle: tokio::task::JoinHandle<()>, // kept alive: dropping aborts the server
}

impl TestServer {
    async fn new(app: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .expect("server failed");
        });
        Self {
            addr,
            _handle: handle,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Create a fixture tenant directory with a stub compose wrapper and
/// credential metadata.
fn extract_job_id(body: &str) -> String {
    let marker = "\"job_id\":";
    let tail = body.rsplit(marker).next().unwrap_or("");
    tail.trim_start()
        .trim_matches(|c| c == '"' || c == '}' || c == ' ')
        .to_string()
}

fn fixture_tenant(root: &Path, slug: &str, service: &str) {
    let dir = root.join(slug);
    std::fs::create_dir_all(&dir).expect("tenant dir");

    let compose = dir.join("compose.sh");
    let mut f = std::fs::File::create(&compose).expect("create compose.sh");
    writeln!(f, "#!/bin/sh").expect("write");
    if std::env::var("FIXTURE_HEALTHY").is_ok() {
        writeln!(
            f,
            "echo 'NAME IMAGE SERVICE CREATED STATUS PORTS'\n\
             echo '{slug}-pg  pg  postgres  1m  Up 1m  '\n\
             echo '{slug}-app  app  app  1m  Up 30s  '"
        )
        .expect("write");
    } else {
        writeln!(
            f,
            "echo 'NAME IMAGE SERVICE CREATED STATUS PORTS'\n\
             echo '{slug}-app  app  app  1m  Up 1m (unhealthy)  '"
        )
        .expect("write");
    }
    drop(f);
    std::fs::set_permissions(&compose, std::fs::Permissions::from_mode(0o755))
        .expect("chmod compose.sh");

    std::fs::write(
        dir.join(".credentials"),
        format!(
            "tenant={slug}\nservice={service}\ndomain=docs.{slug}.example\n\
             local_url=http://127.0.0.1:18080\ninitial_admin_user=admin\n\
             initial_admin_password=neverleak\npostgres_password=alsonot\n"
        ),
    )
    .expect("write credentials");
}

fn fixture_root(healthy: bool) -> (tempfile::TempDir, PathBuf) {
    // Unique per call: tests run in parallel in one process and must not
    // share (or delete each other's) tenant roots.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    // Subprocess state is inherited by the API's env, so signal health via
    // the stub script content itself.
    if healthy {
        std::env::set_var("FIXTURE_HEALTHY", "1");
    } else {
        std::env::remove_var("FIXTURE_HEALTHY");
    }
    fixture_tenant(&root, "acme", "documents");
    fixture_tenant(&root, "globex", "files");
    (dir, root)
}

fn fake_backup_script(dir: &Path, exit_code: i32) -> PathBuf {
    let script = dir.join("fake-tenant-backup.sh");
    let mut f = std::fs::File::create(&script).expect("create backup script");
    writeln!(f, "#!/bin/sh").expect("write");
    writeln!(f, "echo \"snapshotting $1\" >&2").expect("write");
    writeln!(f, "exit {exit_code}").expect("write");
    drop(f);
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod backup script");
    script
}

async fn serve(root: PathBuf, backup_script: PathBuf) -> String {
    serve_with_ledger(root, backup_script, None).await
}

async fn serve_with_ledger(
    root: PathBuf,
    backup_script: PathBuf,
    ledger_db: Option<PathBuf>,
) -> String {
    let registry = Arc::new(Registry::new());
    let jobs_dir = tempfile::tempdir().expect("jobs tmp");
    let _ = &ledger_db;
    let state = Arc::new(
        AppState::new(
            root,
            backup_script,
            registry,
            jobs_dir.path().to_path_buf(),
            Some("test-token".to_string()),
            false,
        )
        .expect("state from fixture"),
    );
    let server = TestServer::new(state.router()).await;
    server.base_url()
}

#[tokio::test]
async fn healthz_is_ok() {
    let (_guard, root) = fixture_root(true);
    let url = serve(root, PathBuf::from("/bin/true")).await;
    let body = reqwest_get(&format!("{url}/healthz")).await;
    // Estate healthkit contract: JSON liveness with a status enum.
    assert_eq!(body, "{\"status\":\"healthy\"}");
}

#[tokio::test]
async fn lists_tenants_with_status_and_no_secrets() {
    let (_guard, root) = fixture_root(true);
    let url = serve(root, PathBuf::from("/bin/true")).await;
    let body = reqwest_get(&format!("{url}/api/tenants")).await;
    assert!(body.contains("\"acme\""), "tenant listed: {body}");
    assert!(body.contains("documents"));
    assert!(body.contains("\"healthy\":true"), "status: {body}");
    // The hard rule: credentials never leak over the wire.
    assert!(!body.contains("neverleak"), "secret leaked: {body}");
    assert!(!body.contains("alsonot"), "secret leaked: {body}");
}

#[tokio::test]
async fn unknown_tenant_is_404() {
    let (_guard, root) = fixture_root(true);
    let url = serve(root, PathBuf::from("/bin/true")).await;
    let status = reqwest_status(&format!("{url}/api/tenants/nope")).await;
    assert_eq!(status, 404);
    // Path traversal is not a tenant.
    let status = reqwest_status(&format!("{url}/api/tenants/..%2F..%2Fetc")).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn backup_runs_script_and_reports_outcome() {
    let (_guard, root) = fixture_root(true);
    let tmp = tempfile::tempdir().expect("tmp");
    let script = fake_backup_script(tmp.path(), 0);
    let url = serve(root, script).await;

    let body = reqwest_post(&format!("{url}/api/tenants/acme/backup")).await;
    let job_id = extract_job_id(&body);
    assert!(!job_id.is_empty(), "202 + job id expected, got: {body}");

    // Async: poll the job endpoint until terminal.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut record = String::new();
    while std::time::Instant::now() < deadline {
        record = reqwest_get(&format!("{url}/api/jobs/{job_id}")).await;
        if record.contains("\"succeeded\"") || record.contains("\"failed\"") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(record.contains("\"succeeded\""), "final record: {record}");
    assert!(
        record.contains("snapshotting acme"),
        "stderr captured: {record}"
    );
}

#[tokio::test]
async fn backup_script_failure_is_reported_not_500() {
    let (_guard, root) = fixture_root(true);
    let tmp = tempfile::tempdir().expect("tmp");
    let script = fake_backup_script(tmp.path(), 3);
    let url = serve(root, script).await;

    let body = reqwest_post(&format!("{url}/api/tenants/acme/backup")).await;
    let job_id = extract_job_id(&body);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut record = String::new();
    while std::time::Instant::now() < deadline {
        record = reqwest_get(&format!("{url}/api/jobs/{job_id}")).await;
        if record.contains("\"failed\"") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(record.contains("\"state\":\"failed\""), "record: {record}");
    assert!(record.contains("\"exit_code\":3"), "exit code: {record}");
}

#[tokio::test]
async fn metrics_endpoint_exposes_series() {
    let (_guard, root) = fixture_root(true);
    let url = serve(root, PathBuf::from("/bin/true")).await;
    let body = reqwest_get(&format!("{url}/metrics")).await;
    assert!(body.contains("hosting_backups_total"), "metrics: {body}");
    assert!(body.contains("hosting_tenants_up"), "metrics: {body}");
}

// --- thin HTTP helpers (reqwest is testkit's dev-only dep; keep our own
// deps minimal by using it directly here as a dev-dependency) -----------

async fn reqwest_get(url: &str) -> String {
    reqwest::Client::new()
        .get(url)
        .bearer_auth("test-token")
        .send()
        .await
        .expect("GET")
        .text()
        .await
        .expect("body")
}

async fn reqwest_status(url: &str) -> u16 {
    reqwest::Client::new()
        .get(url)
        .bearer_auth("test-token")
        .send()
        .await
        .expect("GET")
        .status()
        .as_u16()
}

async fn reqwest_post(url: &str) -> String {
    reqwest::Client::new()
        .post(url)
        .bearer_auth("test-token")
        .send()
        .await
        .expect("POST")
        .text()
        .await
        .expect("body")
}

// --- metering endpoints -------------------------------------------------
//
// Metering is exercised end to end against a stub cAdvisor exposition
// endpoint, so the scrape -> attribute -> price path is covered rather
// than just the parser.

/// Serve a stateful cAdvisor stub: the first scrape reports the baseline
/// counters, later scrapes report advanced counters, so a delta actually
/// exists to measure.
async fn stub_cadvisor() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/metrics",
        axum::routing::get({
            let calls = Arc::clone(&calls);
            move || {
                let calls = Arc::clone(&calls);
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let (cpu, mem) = if n == 0 {
                        (100, 536_870_912_u64)
                    } else {
                        (160, 1_610_612_736_u64)
                    };
                    format!(
                        concat!(
                            "container_cpu_usage_seconds_total{{cpu=\"total\",id=\"/docker/a\",",
                            "name=\"acme-paperless-1\"}} {cpu}\n",
                            "container_memory_working_set_bytes{{id=\"/docker/a\",",
                            "name=\"acme-paperless-1\"}} {mem}\n",
                            "container_cpu_usage_seconds_total{{cpu=\"total\",id=\"/docker/b\",",
                            "name=\"ghost-thing-1\"}} 999\n",
                        ),
                        cpu = cpu,
                        mem = mem
                    )
                }
            }
        }),
    );
    TestServer::new(app).await.base_url()
}

#[tokio::test]
async fn usage_endpoints_report_disabled_when_cadvisor_is_unset() {
    let (root_dir, root) = fixture_root(true);
    let script = fake_backup_script(root_dir.path(), 0);
    std::env::remove_var("HOSTING_CADVISOR_URL");

    let base = serve(root, script).await;
    let client = reqwest::Client::new();

    let res = client
        .get(format!("{base}/api/usage"))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("GET /api/usage");
    assert!(res.status().is_success());
    let body: serde_json::Value = res.json().await.expect("json");
    assert_eq!(
        body.get("metering_enabled"),
        Some(&serde_json::json!(false))
    );
    assert_eq!(body.get("currency"), Some(&serde_json::json!("GBP")));
    assert!(
        body.get("tenants").is_some_and(serde_json::Value::is_array),
        "tenant rows are still reported: {body}"
    );
}

#[tokio::test]
async fn tenant_usage_requires_the_bearer_token() {
    let (root_dir, root) = fixture_root(true);
    let script = fake_backup_script(root_dir.path(), 0);
    let base = serve(root, script).await;

    let res = reqwest::Client::new()
        .get(format!("{base}/api/tenants/acme/usage"))
        .send()
        .await
        .expect("GET without token");
    assert!(
        res.status() == reqwest::StatusCode::UNAUTHORIZED
            || res.status() == reqwest::StatusCode::FORBIDDEN,
        "unauthenticated usage must be refused, got {}",
        res.status()
    );
}

#[tokio::test]
async fn tenant_usage_404s_for_an_unknown_tenant() {
    let (root_dir, root) = fixture_root(true);
    let script = fake_backup_script(root_dir.path(), 0);
    let base = serve(root, script).await;

    let res = reqwest::Client::new()
        .get(format!("{base}/api/tenants/nope/usage"))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("GET unknown tenant");
    assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn metering_scrapes_attributes_and_prices_a_tenant() {
    let (root_dir, root) = fixture_root(true);
    let script = fake_backup_script(root_dir.path(), 0);
    let cadvisor = format!("{}/metrics", stub_cadvisor().await);
    std::env::set_var("HOSTING_CADVISOR_URL", &cadvisor);
    std::env::set_var("HOSTING_CPU_PRICE_PER_CORE_HOUR", "0.01");
    std::env::set_var("HOSTING_MEMORY_PRICE_PER_GIB_HOUR", "0");

    let base = serve(root, script).await;
    let client = reqwest::Client::new();

    // First call establishes the cAdvisor baseline.
    let first: serde_json::Value = client
        .get(format!("{base}/api/tenants/acme/usage"))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("first scrape")
        .json()
        .await
        .expect("json");
    assert_eq!(
        first.get("metering_enabled"),
        Some(&serde_json::json!(true))
    );
    assert!(
        first.get("usage").is_none_or(serde_json::Value::is_null),
        "a baseline scrape yields no usage yet: {first}"
    );

    // Second call folds the new counters into the window. The stub returns
    // the same body, so the delta is exactly the second sample line.
    let second: serde_json::Value = client
        .get(format!("{base}/api/tenants/acme/usage"))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("second scrape")
        .json()
        .await
        .expect("json");
    let cpu = second
        .get("usage")
        .and_then(|u| u.get("cpu_core_seconds"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("0");
    assert!(
        cpu.parse::<f64>().unwrap_or(-1.0) > 0.0,
        "cpu usage should be attributed to acme: {second}"
    );
    // 60 core-seconds at 0.01/core-hour = 0.0001666... GBP, non-zero.
    assert!(
        second
            .get("charge")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|c| c.parse::<f64>().unwrap_or(0.0) > 0.0),
        "charge should be priced: {second}"
    );

    // A container whose project is not a tenant must not be attributed.
    let summary: serde_json::Value = client
        .get(format!("{base}/api/usage"))
        .bearer_auth("test-token")
        .send()
        .await
        .expect("summary")
        .json()
        .await
        .expect("json");
    let rows = summary
        .get("tenants")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        rows.iter()
            .all(|r| r.get("tenant").and_then(serde_json::Value::as_str) != Some("ghost")),
        "unknown container projects must not become tenants: {summary}"
    );

    std::env::remove_var("HOSTING_CADVISOR_URL");
}

#[tokio::test]
async fn api_rate_limit_engages_gcra() {
    // throttle-kit GCRA: quota 10/s per identity. Hammer /healthz? No -
    // /healthz is open and unthrottled by design; hammer an /api route.
    let (root_dir, root) = fixture_root(true);
    let script = fake_backup_script(root_dir.path(), 0);
    let base = serve(root, script).await;
    let client = reqwest::Client::new();

    // The tests run fast enough that >10 requests land inside the burst
    // window; at least one must draw a 429 with the throttle-kit body.
    let mut codes = Vec::new();
    for _ in 0..14 {
        let res = client
            .get(format!("{base}/api/tenants"))
            .header("X-Forwarded-For", "203.0.113.7")
            .bearer_auth("test-token")
            .send()
            .await
            .expect("request");
        codes.push(res.status().as_u16());
    }
    assert!(
        codes.contains(&429),
        "GCRA must engage inside the burst window: {codes:?}"
    );
}

/// OIDC fixture: a loopback identity provider serving discovery and
/// generated ES256 key, plus a signer for tokens. (Round 7 iter 64.)
use base64::Engine as _;

struct OidcFixture {
    base: String,
    signer: jsonwebtoken::EncodingKey,
    kid: &'static str,
}

impl OidcFixture {
    async fn start(roles: &[&str]) -> Self {
        use p256::ecdsa::SigningKey;
        use p256::pkcs8::EncodePrivateKey;
        use rand_core::OsRng;

        let secret = SigningKey::random(&mut OsRng);
        let signer = jsonwebtoken::EncodingKey::from_ec_pem(
            secret
                .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
                .expect("pkcs8")
                .as_bytes(),
        )
        .expect("encoding key");

        // Public point -> EC2 JWK coordinates.
        let point = secret.verifying_key().to_encoded_point(false);
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let jwks = serde_json::json!({
            "keys": [{
                "kty": "EC", "crv": "P-256", "kid": "probe",
                "x": b64(point.x().unwrap().as_slice()),
                "y": b64(point.y().unwrap().as_slice()),
            }]
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("idp bind");
        let addr = listener.local_addr().expect("idp addr");
        let issuer = format!("http://{addr}/authbox");
        // OidcDiscovery requires the full endpoint set; serve the shapes
        // the validator's clients use.
        let discovery_body = serde_json::json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/auth"),
            "token_endpoint": format!("{issuer}/token"),
            "userinfo_endpoint": format!("{issuer}/userinfo"),
            "jwks_uri": format!("{issuer}/jwks.json"),
        })
        .to_string();
        let jwks_body = jwks.to_string();
        let app = axum::Router::new().nest(
            "/authbox",
            axum::Router::new()
                .route(
                    "/.well-known/openid-configuration",
                    axum::routing::get(move || {
                        let body = discovery_body.clone();
                        async move { body }
                    }),
                )
                .route(
                    "/jwks.json",
                    axum::routing::get(move || {
                        let body = jwks_body.clone();
                        async move { body }
                    }),
                ),
        );
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .expect("idp serve");
        });
        // Fixture self-check: the discovery endpoint must serve JSON.
        let probe = reqwest::get(format!("{issuer}/.well-known/openid-configuration"))
            .await
            .expect("idp reachable");
        let status = probe.status();
        let ctype = probe
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("?")
            .to_string();
        let body = probe.text().await.unwrap_or_default();
        eprintln!("IDP self-check: {status} {ctype} {body}");
        let _ = roles;
        Self {
            base: format!("http://{addr}/authbox"),
            signer,
            kid: "probe",
        }
    }

    fn token(&self, sub: &str, roles: &[&str]) -> String {
        let claims = serde_json::json!({
            "iss": self.base,
            "sub": sub,
            "aud": "hosting-api",
            "exp": chrono::Utc::now().timestamp() + 600,
            "realm_access": { "roles": roles },
        });
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.kid.to_string());
        jsonwebtoken::encode(&header, &claims, &self.signer).expect("sign")
    }
}

#[tokio::test]
async fn authbox_issued_jwt_grants_api_access_by_role() {
    // Serial: AppState::new reads process env at construction.
    let idp = OidcFixture::start(&["Admin"]).await;
    // SAFETY (test-only): single-threaded env mutation against the
    // fixture's own variables; other tests do not read these keys.
    // Ambient HTTP(S)_PROXY env would route the loopback JWKS fetch
    // through a corporate proxy (reqwest honors it by default).
    std::env::set_var("NO_PROXY", "*");
    std::env::set_var("no_proxy", "*");
    std::env::set_var("HOSTING_OIDC_ISSUER", idp.base.clone());
    std::env::set_var("HOSTING_OIDC_AUDIENCE", "hosting-api");

    let (_guard, root) = fixture_root(true);
    let script_dir = tempfile::tempdir().expect("script tmp");
    let script = script_dir.path().join("tenant-backup.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 0").expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let registry = Arc::new(Registry::new());
    let jobs_dir = tempfile::tempdir().expect("jobs tmp");
    let state = Arc::new(
        AppState::new(
            root.clone(),
            script.clone(),
            registry,
            jobs_dir.path().to_path_buf(),
            Some("legacy-token".to_string()),
            false,
        )
        .expect("state"),
    );
    assert!(
        state.oidc.is_some(),
        "OIDC validator must be constructed when HOSTING_OIDC_ISSUER is set"
    );
    let server = TestServer::new(state.router()).await;
    let base = server.base_url();

    // Admin-role JWT from the fixture issuer: accepted.
    let admin = idp.token("probe-svc", &["Admin"]);
    let res = reqwest::Client::new()
        .get(format!("{base}/api/tenants"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("request");
    assert_eq!(res.status(), 200, "Admin JWT must grant /api access");

    // Viewer-role JWT from the same issuer: rejected (role gate).
    let viewer = idp.token("probe-svc", &["Viewer"]);
    let res = reqwest::Client::new()
        .get(format!("{base}/api/tenants"))
        .header("Authorization", format!("Bearer {viewer}"))
        .send()
        .await
        .expect("request");
    assert_eq!(res.status(), 401, "non-Admin roles must not pass");

    // The legacy static token still works (break-glass path intact).
    let res = reqwest::Client::new()
        .get(format!("{base}/api/tenants"))
        .header("Authorization", "Bearer legacy-token")
        .send()
        .await
        .expect("request");
    assert_eq!(res.status(), 200, "legacy token must keep working");
}

/// The ledger endpoint serves the durable charge statement through the
/// real router (round 8 iter 75): a posting recorded on the state's
/// ledger must appear in the HTTP response with the derived balance.
#[tokio::test]
async fn ledger_endpoint_serves_recorded_windows() {
    #![allow(clippy::indexing_slicing)]
    use hosting_api::usage::{MeteringConfig, TenantUsage};

    let (_guard, root) = fixture_root(true);
    let tmp = tempfile::tempdir().expect("script tmp");
    let script = fake_backup_script(tmp.path(), 0);
    let registry = Arc::new(Registry::new());
    let jobs_dir = tempfile::tempdir().expect("jobs tmp");
    let state = Arc::new(
        AppState::new(
            root,
            script,
            registry,
            jobs_dir.path().to_path_buf(),
            Some("test-token".to_string()),
            false,
        )
        .expect("state from fixture"),
    );

    // Record one closed metering window directly on the state's ledger.
    let cfg = MeteringConfig {
        currency: billing_kit::Currency::GBP,
        cpu_price_per_core_hour: rust_decimal::Decimal::from(1),
        memory_price_per_gib_hour: rust_decimal::Decimal::ZERO,
        discount_percent: rust_decimal::Decimal::ZERO,
    };
    let usage = TenantUsage {
        tenant: "acme".to_string(),
        window_start: chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("ts"),
        window_end: chrono::DateTime::from_timestamp(1_700_000_000 + 3600, 0).expect("ts"),
        cpu_core_seconds: rust_decimal::Decimal::from(3600), // 1 core-hour = £0.01
        memory_gibibyte_seconds: rust_decimal::Decimal::ZERO,
    };
    let posting = state
        .ledger
        .record_window(&usage, &cfg)
        .await
        .expect("record window")
        .expect("posting accepted");

    let server = TestServer::new(state.router()).await;
    let base = server.base_url();
    let res = reqwest::Client::new()
        .get(format!("{base}/api/tenants/acme/ledger"))
        .header("Authorization", "Bearer test-token")
        .send()
        .await
        .expect("request");
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.expect("json");
    assert_eq!(body["tenant"], "acme");
    let postings = body["postings"].as_array().expect("postings array");
    assert_eq!(postings.len(), 1, "the recorded window appears");
    assert_eq!(
        postings[0]["id"],
        posting.id().to_string(),
        "the posting id round-trips"
    );

    // Unknown tenants are 404 (the fixture root only knows acme).
    let res = reqwest::Client::new()
        .get(format!("{base}/api/tenants/ghost/ledger"))
        .header("Authorization", "Bearer test-token")
        .send()
        .await
        .expect("request");
    assert_eq!(res.status(), 404);
}
