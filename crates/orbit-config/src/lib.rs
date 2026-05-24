//! **orbit-config** — typed 12-factor configuration.
//!
//! Layered loading via `figment`:
//!   1. `OrbitConfig::default()` baseline
//!   2. Optional TOML file (e.g. `orbit.toml`)
//!   3. `ORBIT_*` environment variables (highest priority)
//!
//! Durations accept human-readable strings ("300s", "10m") in TOML/env.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![cfg_attr(not(test), forbid(unsafe_code))]
#![warn(missing_docs)]

use std::path::Path;
use std::time::Duration;

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Top-level orbit-rs configuration.
///
/// All fields default to safe values matching the current orbit-server CLI
/// args. New sections (`[catalog]`, `[resilience]`) are added as nested
/// structs in subsequent weeks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrbitConfig {
    /// gRPC bind address.
    pub bind: String,
    /// SQLite database URL.
    pub db: String,
    /// Optional canonicalised data root (for the path-traversal guard).
    pub data_root: Option<String>,
    /// Max decoded gRPC body bytes.
    pub max_msg_bytes: usize,
    /// Per-connection concurrency limit.
    pub concurrency: usize,
    /// Per-request server timeout. Accepts humantime ("300s", "5m").
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    /// Per-job Polars-SQL timeout. Accepts humantime.
    #[serde(with = "humantime_serde")]
    pub query_timeout: Duration,
    /// Optional bearer token (required when bind is non-loopback).
    pub auth_token: Option<String>,
}

impl Default for OrbitConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:9876".into(),
            db: "sqlite://./data/orbit.db?mode=rwc".into(),
            data_root: None,
            max_msg_bytes: 4 * 1024 * 1024,
            concurrency: 32,
            request_timeout: Duration::from_secs(600),
            query_timeout: Duration::from_secs(300),
            auth_token: None,
        }
    }
}

impl OrbitConfig {
    /// Load the layered configuration:
    ///
    /// 1. Defaults
    /// 2. Optional TOML file at `toml_path` (skipped if `None` or missing)
    /// 3. `ORBIT_*` environment variables (e.g. `ORBIT_BIND`)
    ///
    /// The last layer wins for each field.
    pub fn load(toml_path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut fig = Figment::from(Serialized::defaults(Self::default()));
        if let Some(p) = toml_path {
            if p.exists() {
                fig = fig.merge(Toml::file(p));
            }
        }
        fig = fig.merge(Env::prefixed("ORBIT_"));
        let cfg: Self = fig.extract().map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load a TOML file with no env overrides. Useful for tests that need
    /// deterministic config without polluting process environment.
    pub fn load_toml(toml_path: &Path) -> Result<Self, ConfigError> {
        let cfg: Self = Figment::from(Serialized::defaults(Self::default()))
            .merge(Toml::file(toml_path))
            .extract()
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Apply environment-variable overrides on top of an existing config.
    ///
    /// Maintained for backward-compat; new code should call [`Self::load`].
    pub fn with_env_overrides(self) -> Result<Self, ConfigError> {
        let cfg: Self = Figment::from(Serialized::defaults(self))
            .merge(Env::prefixed("ORBIT_"))
            .extract()
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate the loaded config for invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_msg_bytes == 0 {
            return Err(ConfigError::Invalid("max_msg_bytes must be > 0".into()));
        }
        if self.concurrency == 0 {
            return Err(ConfigError::Invalid("concurrency must be > 0".into()));
        }
        if self.request_timeout.is_zero() {
            return Err(ConfigError::Invalid("request_timeout must be > 0".into()));
        }
        if self.query_timeout.is_zero() {
            return Err(ConfigError::Invalid("query_timeout must be > 0".into()));
        }
        Ok(())
    }
}

/// Config-loading errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to parse a config field.
    #[error("config parse: {0}")]
    Parse(String),
    /// Config failed an invariant after loading.
    #[error("config invalid: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".toml").tempfile().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    /// Run a closure with a clean ORBIT_* env so tests don't pollute each
    /// other. The environment is process-global, so we serialise these
    /// tests via a Mutex.
    fn with_clean_env<F: FnOnce()>(f: F) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Snapshot and remove all ORBIT_* env vars, then restore.
        let snapshot: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| k.starts_with("ORBIT_"))
            .collect();
        for (k, _) in &snapshot {
            unsafe { std::env::remove_var(k); }
        }
        f();
        for (k, v) in snapshot {
            unsafe { std::env::set_var(k, v); }
        }
    }

    #[test]
    fn default_values_match_orbit_server_defaults() {
        let c = OrbitConfig::default();
        assert_eq!(c.bind, "127.0.0.1:9876");
        assert_eq!(c.max_msg_bytes, 4 * 1024 * 1024);
        assert_eq!(c.concurrency, 32);
        assert_eq!(c.request_timeout, Duration::from_secs(600));
        assert_eq!(c.query_timeout, Duration::from_secs(300));
    }

    #[test]
    fn default_validates() {
        OrbitConfig::default().validate().expect("default must validate");
    }

    #[test]
    fn zero_max_msg_bytes_fails_validate() {
        let mut c = OrbitConfig::default();
        c.max_msg_bytes = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn zero_concurrency_fails_validate() {
        let mut c = OrbitConfig::default();
        c.concurrency = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn zero_request_timeout_fails_validate() {
        let mut c = OrbitConfig::default();
        c.request_timeout = Duration::ZERO;
        assert!(c.validate().is_err());
    }

    #[test]
    fn load_toml_overrides_defaults() {
        with_clean_env(|| {
            let toml = r#"
bind = "0.0.0.0:1234"
concurrency = 64
request_timeout = "120s"
"#;
            let f = write_tmp(toml);
            let c = OrbitConfig::load_toml(f.path()).expect("load");
            assert_eq!(c.bind, "0.0.0.0:1234");
            assert_eq!(c.concurrency, 64);
            assert_eq!(c.request_timeout, Duration::from_secs(120));
            // Untouched fields fall back to default.
            assert_eq!(c.max_msg_bytes, 4 * 1024 * 1024);
        });
    }

    #[test]
    fn humantime_duration_accepts_minutes() {
        with_clean_env(|| {
            let toml = "query_timeout = \"5m\"\n";
            let f = write_tmp(toml);
            let c = OrbitConfig::load_toml(f.path()).unwrap();
            assert_eq!(c.query_timeout, Duration::from_secs(300));
        });
    }

    #[test]
    fn env_overrides_toml() {
        with_clean_env(|| {
            let toml = "bind = \"file-bound:1\"\n";
            let f = write_tmp(toml);
            unsafe { std::env::set_var("ORBIT_BIND", "env-bound:2"); }
            let c = OrbitConfig::load(Some(f.path())).unwrap();
            assert_eq!(c.bind, "env-bound:2", "env must win over file");
            unsafe { std::env::remove_var("ORBIT_BIND"); }
        });
    }

    #[test]
    fn missing_toml_falls_back_to_defaults_plus_env() {
        with_clean_env(|| {
            let nonexistent = std::path::Path::new("/tmp/no-such-orbit-file.toml");
            let c = OrbitConfig::load(Some(nonexistent)).unwrap();
            assert_eq!(c, OrbitConfig::default());
        });
    }

    #[test]
    fn load_none_path_is_defaults_plus_env() {
        with_clean_env(|| {
            let c = OrbitConfig::load(None).unwrap();
            assert_eq!(c, OrbitConfig::default());
        });
    }

    #[test]
    fn validate_rejects_loaded_invalid_concurrency() {
        with_clean_env(|| {
            let toml = "concurrency = 0\n";
            let f = write_tmp(toml);
            let r = OrbitConfig::load_toml(f.path());
            assert!(r.is_err());
        });
    }

    #[test]
    fn with_env_overrides_still_works() {
        with_clean_env(|| {
            unsafe { std::env::set_var("ORBIT_CONCURRENCY", "16"); }
            let c = OrbitConfig::default().with_env_overrides().unwrap();
            assert_eq!(c.concurrency, 16);
            unsafe { std::env::remove_var("ORBIT_CONCURRENCY"); }
        });
    }

    #[test]
    fn serde_roundtrip() {
        let c = OrbitConfig::default();
        let s = serde_json::to_string(&c).unwrap();
        let back: OrbitConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
    }
}
