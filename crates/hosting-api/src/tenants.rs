//! Tenant discovery and state over the on-disk tenant layout.
//!
//! A tenant is a directory under `TENANT_ROOT` (default `/srv/tenants`)
//! containing:
//!
//! - `compose.sh` — the generated compose wrapper (provisioned tenants only)
//! - `.credentials` — `key=value` metadata written by the provisioner
//!
//! The API reads state by shelling out to `compose.sh ps` with a hard
//! timeout. Secrets from `.credentials` are parsed but **never** serialised:
//! [`Credentials`] only exposes the non-sensitive fields, and the compiler
//! enforces that because the secret fields are private to this module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

/// How long `compose.sh ps` may run before we declare the tenant broken.
const COMPOSE_PS_TIMEOUT: Duration = Duration::from_secs(15);

/// Metadata parsed from a tenant's `.credentials` file.
///
/// Secret fields (`initial_admin_password`, `postgres_password`,
/// `redis_password`, `paperless_secret_key`) are captured into
/// [`RawCredentials`] but deliberately not reachable from this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Credentials {
    /// Tenant slug (directory name).
    pub tenant: String,
    /// Deployed service template, e.g. `documents`.
    pub service: String,
    /// Public hostname, e.g. `docs.acme.example`.
    pub domain: String,
    /// Local origin, e.g. `http://127.0.0.1:18080`.
    pub local_url: String,
    /// Initial superuser name (password never exposed).
    pub initial_admin_user: String,
}

/// Everything in `.credentials`, including secrets. Never leaves this
/// module; used only for integrity checks in tests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct RawCredentials(BTreeMap<String, String>);

impl RawCredentials {
    fn parse(text: &str) -> Self {
        let mut map = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                map.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        RawCredentials(map)
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Project to the safe, publishable view.
    fn to_credentials(&self, tenant: &str) -> Option<Credentials> {
        Some(Credentials {
            tenant: tenant.to_string(),
            service: self.get("service")?.to_string(),
            domain: self.get("domain")?.to_string(),
            local_url: self.get("local_url")?.to_string(),
            initial_admin_user: self.get("initial_admin_user")?.to_string(),
        })
        .filter(|c| !c.service.is_empty() && !c.domain.is_empty())
    }
}

/// Aggregated tenant state as reported by `compose.sh ps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TenantStatus {
    /// Number of services defined in the tenant's compose project.
    pub services: usize,
    /// Services currently `Up`.
    pub up: usize,
    /// Services `unhealthy` or restarting.
    pub degraded: usize,
    /// True when `up > 0` and `degraded == 0`.
    pub healthy: bool,
}

/// A discovered tenant: metadata plus live status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tenant {
    /// Safe credential metadata (no secrets).
    #[serde(flatten)]
    pub credentials: Credentials,
    /// Compose project state.
    pub status: TenantStatus,
}

/// Root directory that holds all tenant directories.
#[derive(Debug, Clone)]
pub struct TenantRoot(PathBuf);

impl TenantRoot {
    /// Validate and wrap a tenant root path.
    ///
    /// # Errors
    /// Returns [`crate::error::ApiError::Config`] when the path does not
    /// exist or is not a directory.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, crate::error::ApiError> {
        let path = path.into();
        if !path.is_dir() {
            return Err(crate::error::ApiError::Config(format!(
                "tenant root {} is not a directory",
                path.display()
            )));
        }
        Ok(Self(path))
    }

    /// The root path itself (for components that re-validate on their own,
    /// e.g. the nightly sweep's supervisor closure).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Tenant directory for a slug, rejected unless it is a real,
    /// provisioned tenant (has `compose.sh`).
    ///
    /// # Errors
    /// [`crate::error::ApiError::TenantNotFound`] for unknown or
    /// unprovisioned slugs.
    pub fn tenant_dir(&self, tenant: &str) -> Result<PathBuf, crate::error::ApiError> {
        if !is_valid_slug(tenant) {
            return Err(crate::error::ApiError::TenantNotFound(tenant.to_string()));
        }
        let dir = self.0.join(tenant);
        if dir.join("compose.sh").is_file() {
            Ok(dir)
        } else {
            Err(crate::error::ApiError::TenantNotFound(tenant.to_string()))
        }
    }

    /// All provisioned tenants under this root, sorted by slug.
    #[must_use]
    pub fn all(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.0) else {
            return Vec::new();
        };
        let mut tenants: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| self.tenant_dir(name).is_ok())
            .collect();
        tenants.sort();
        tenants
    }

    /// Parse one tenant's `.credentials` into the safe view.
    ///
    /// # Errors
    /// [`crate::error::ApiError::TenantNotFound`] when metadata is absent
    /// or unparseable.
    pub fn credentials(&self, tenant: &str) -> Result<Credentials, crate::error::ApiError> {
        let dir = self.tenant_dir(tenant)?;
        let text = std::fs::read_to_string(dir.join(".credentials"))
            .map_err(|_| crate::error::ApiError::TenantNotFound(tenant.to_string()))?;
        RawCredentials::parse(&text)
            .to_credentials(tenant)
            .ok_or_else(|| crate::error::ApiError::TenantNotFound(tenant.to_string()))
    }

    /// Query `compose.sh ps` for a tenant and summarise the output.
    ///
    /// # Errors
    /// [`crate::error::ApiError::Subprocess`] when the wrapper fails or
    /// exceeds [`COMPOSE_PS_TIMEOUT`].
    pub async fn status(&self, tenant: &str) -> Result<TenantStatus, crate::error::ApiError> {
        let dir = self.tenant_dir(tenant)?;
        let child = Command::new(dir.join("compose.sh"))
            .arg("ps")
            .current_dir(&dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| crate::error::ApiError::Subprocess(e.to_string()))?;

        let output = match tokio::time::timeout(COMPOSE_PS_TIMEOUT, child.wait_with_output()).await
        {
            Ok(Ok(out)) if out.status.success() => out,
            Ok(Ok(out)) => {
                return Err(crate::error::ApiError::Subprocess(format!(
                    "compose ps exited with {}",
                    out.status
                )));
            }
            Ok(Err(e)) => return Err(crate::error::ApiError::Subprocess(e.to_string())),
            Err(_) => {
                return Err(crate::error::ApiError::Subprocess(format!(
                    "compose ps exceeded {}s timeout",
                    COMPOSE_PS_TIMEOUT.as_secs()
                )));
            }
        };

        let text = String::from_utf8_lossy(&output.stdout);
        Ok(summarise_compose_ps(&text))
    }
}

/// Slug rules mirror the provisioner: `[a-z0-9][a-z0-9-]{1,30}`.
fn is_valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {
            slug.len() >= 2
                && slug.len() <= 31
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }
        _ => false,
    }
}

/// Pure summariser over `docker compose ps` table output — unit tested
/// without Docker.
fn summarise_compose_ps(text: &str) -> TenantStatus {
    // Table header row is dropped; data rows follow.
    let rows = text
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("NAME"))
        .skip(1)
        .filter(|l| !l.trim().is_empty());

    let mut services = 0usize;
    let mut up = 0usize;
    let mut degraded = 0usize;
    for row in rows {
        let lower = row.to_ascii_lowercase();
        services += 1;
        if lower.contains("unhealthy") || lower.contains("restarting") {
            degraded += 1;
        } else if lower.contains("up ") || lower.contains("running") {
            up += 1;
        }
    }
    TenantStatus {
        services,
        up,
        degraded,
        healthy: up > 0 && degraded == 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic)]
    use super::*;

    #[test]
    fn summarises_empty_output() {
        let s = summarise_compose_ps("");
        assert_eq!(s.services, 0);
        assert!(!s.healthy);
    }

    #[test]
    fn summarises_healthy_stack() {
        let s = summarise_compose_ps(
            "NAME              IMAGE   COMMAND  SERVICE  CREATED  STATUS   PORTS\n\
             t-docs-postgres   pg      \"x\"      postgres 1m ago   Up 1m    \n\
             t-docs-webserver  pp      \"x\"      paperless 1m ago  Up 30s   \n",
        );
        assert_eq!(s.services, 2);
        assert_eq!(s.up, 2);
        assert_eq!(s.degraded, 0);
        assert!(s.healthy);
    }

    #[test]
    fn summarises_degraded_stack() {
        let s = summarise_compose_ps(
            "NAME    IMAGE  COMMAND  SERVICE  CREATED  STATUS             PORTS\n\
             t-redis r      \"x\"      redis    1m ago   Up 1m (unhealthy)  \n\
             t-app   a      \"x\"      app      1m ago   Restarting (1)     \n",
        );
        assert_eq!(s.services, 2);
        assert_eq!(s.up, 0);
        assert_eq!(s.degraded, 2);
        assert!(!s.healthy);
    }

    #[test]
    fn parses_credentials_and_hides_secrets() {
        let raw = RawCredentials::parse(
            "# comment\nservice=documents\ndomain=docs.acme.example\n\
             local_url=http://127.0.0.1:18080\ninitial_admin_user=admin\n\
             initial_admin_password=supersecret\npostgres_password=alsonot\n",
        );
        let creds = raw.to_credentials("acme").expect("valid metadata");
        assert_eq!(creds.service, "documents");
        assert_eq!(creds.tenant, "acme");
        // The safe view is a struct with no password field at all.
        let json = serde_json::to_string(&creds).expect("serialisable");
        assert!(!json.contains("supersecret"));
        assert!(!json.contains("alsonot"));
    }

    #[test]
    fn rejects_incomplete_credentials() {
        let raw = RawCredentials::parse("service=documents\n");
        assert!(raw.to_credentials("acme").is_none());
    }

    #[test]
    fn slug_validation() {
        assert!(is_valid_slug("acme"));
        assert!(is_valid_slug("acme-corp-2"));
        assert!(!is_valid_slug("-leading"));
        assert!(!is_valid_slug("UPPER"));
        assert!(!is_valid_slug("a"));
        assert!(!is_valid_slug("../../etc"));
    }
}
