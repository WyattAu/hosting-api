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
        Self { addr, _handle: handle }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Create a fixture tenant directory with a stub compose wrapper and
/// credential metadata.
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
    let registry = Arc::new(Registry::new());
    let state = Arc::new(AppState::new(root, backup_script, registry).expect("state from fixture"));
    let server = TestServer::new(state.router()).await;
    server.base_url()
}

#[tokio::test]
async fn healthz_is_ok() {
    let (_guard, root) = fixture_root(true);
    let url = serve(root, PathBuf::from("/bin/true")).await;
    let body = reqwest_get(&format!("{url}/healthz")).await;
    assert_eq!(body, "ok");
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
    assert!(body.contains("\"success\":true"), "outcome: {body}");
    assert!(
        body.contains("snapshotting acme"),
        "stderr captured: {body}"
    );
}

#[tokio::test]
async fn backup_script_failure_is_reported_not_500() {
    let (_guard, root) = fixture_root(true);
    let tmp = tempfile::tempdir().expect("tmp");
    let script = fake_backup_script(tmp.path(), 3);
    let url = serve(root, script).await;

    let body = reqwest_post(&format!("{url}/api/tenants/acme/backup")).await;
    assert!(body.contains("\"success\":false"), "outcome: {body}");
    assert!(body.contains("\"code\":3"), "exit code: {body}");
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
    reqwest::get(url)
        .await
        .expect("GET")
        .text()
        .await
        .expect("body")
}

async fn reqwest_status(url: &str) -> u16 {
    reqwest::get(url).await.expect("GET").status().as_u16()
}

async fn reqwest_post(url: &str) -> String {
    reqwest::Client::new()
        .post(url)
        .send()
        .await
        .expect("POST")
        .text()
        .await
        .expect("body")
}
