//! Configuration and builder types for the RGraph engine.
//!
//! [`Config`] holds every tunable parameter of the database.  It can be
//! constructed programmatically via [`GraphBuilder`], loaded from a TOML
//! file, or merged from environment variables.
//!
//! [`GraphBuilder`] follows the consuming builder pattern so that invalid
//! configurations are rejected at `build()` time rather than at runtime.

use crate::error::{RGraphError, Result};
use std::path::{Path, PathBuf};

/// Supported graph data models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[derive(Default)]
pub enum GraphMode {
    /// Label Property Graph (default).
    #[default]
    Lpg,
    /// Resource Description Framework.
    Rdf,
}


impl std::str::FromStr for GraphMode {
    type Err = RGraphError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "LPG" => Ok(GraphMode::Lpg),
            "RDF" => Ok(GraphMode::Rdf),
            _ => Err(RGraphError::Argument(format!(
                "invalid graph mode '{}' (expected LPG or RDF)",
                s
            ).into())),
        }
    }
}

/// Complete runtime configuration for an RGraph database.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Filesystem path to the database directory.
    pub database_path: PathBuf,

    /// Graph data model: LPG or RDF.
    pub graph_mode: GraphMode,

    /// Page cache size in megabytes.
    pub page_cache_size_mb: usize,

    /// WAL buffer cache size in megabytes.
    pub wal_cache_size_mb: usize,

    /// Maximum concurrent client connections (server mode).
    pub max_connections: usize,

    /// Number of async worker threads (0 = num_cpus).
    pub worker_threads: usize,

    /// Number of CPU-bound rayon threads (0 = num_cpus).
    pub cpu_pool_threads: usize,

    /// Default page size in bytes.
    pub default_page_size: usize,

    /// Enable on-disk compression for pages.
    pub enable_compression: bool,

    /// Enable TCK strict mode (extra validation, no extensions).
    pub tck_mode: bool,

    /// Enable O_DIRECT for data file I/O.
    pub use_odirect: bool,

    /// Enable TLS for the server listener.
    ///
    /// When `true`, both [`Config::tls_cert_path`] and
    /// [`Config::tls_key_path`] must be set; validation rejects the
    /// contradiction otherwise.
    pub tls_enabled: bool,

    /// Path to the TLS certificate chain (PEM).  Required when
    /// [`Config::tls_enabled`] is `true`.
    pub tls_cert_path: Option<PathBuf>,

    /// Path to the TLS private key (PEM).  Required when
    /// [`Config::tls_enabled`] is `true`.
    pub tls_key_path: Option<PathBuf>,
}

/// Upper bound on cache sizes in megabytes (1 TiB).  Larger values almost
/// certainly indicate a misconfiguration (e.g. bytes supplied where
/// megabytes were expected).
const MAX_CACHE_SIZE_MB: usize = 1024 * 1024;

/// Upper bound on the number of concurrent connections.
const MAX_CONNECTIONS_LIMIT: usize = 1_000_000;

/// Upper bound on configurable thread-pool sizes.  `0` always means
/// "derive from the number of CPUs" and is accepted regardless of this
/// cap.
const MAX_THREADS: usize = 4096;

impl Default for Config {
    fn default() -> Self {
        Self {
            database_path: PathBuf::from("rgraph.db"),
            graph_mode: GraphMode::Lpg,
            page_cache_size_mb: 256,
            wal_cache_size_mb: 64,
            max_connections: 1024,
            worker_threads: 0,
            cpu_pool_threads: 0,
            // The storage engine operates on a single fixed page size; the
            // default mirrors it so that `Config::default()` is always
            // valid.  See `crate::storage::page::PAGE_SIZE`.
            default_page_size: crate::storage::page::PAGE_SIZE,
            enable_compression: true,
            tck_mode: false,
            use_odirect: false,
            tls_enabled: false,
            tls_cert_path: None,
            tls_key_path: None,
        }
    }
}

impl Config {
    /// Load configuration from a TOML file.
    ///
    /// Missing fields fall back to [`Config::default`].
    pub fn from_toml(path: impl AsRef<Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| RGraphError::Io(format!("failed to read config file: {}", e).into()))?;
        Self::from_toml_str(&text)
    }

    /// Parse configuration from a TOML string.
    ///
    /// This is a lightweight hand-rolled parser that recognises the subset
    /// of TOML used by RGraph configuration.  It supports `key = value`
    /// assignments and string / integer / boolean literals.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let mut cfg = Config::default();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| RGraphError::Argument(format!("invalid config line: {}", line).into()))?;
            let key = key.trim();
            let value = value.trim();
            match key {
                "database_path" => {
                    cfg.database_path = PathBuf::from(parse_string(value)?);
                }
                "graph_mode" => {
                    cfg.graph_mode = parse_string(value)?.parse()?;
                }
                "page_cache_size_mb" => {
                    cfg.page_cache_size_mb = parse_usize(value)?;
                }
                "wal_cache_size_mb" => {
                    cfg.wal_cache_size_mb = parse_usize(value)?;
                }
                "max_connections" => {
                    cfg.max_connections = parse_usize(value)?;
                }
                "worker_threads" => {
                    cfg.worker_threads = parse_usize(value)?;
                }
                "cpu_pool_threads" => {
                    cfg.cpu_pool_threads = parse_usize(value)?;
                }
                "default_page_size" => {
                    cfg.default_page_size = parse_usize(value)?;
                }
                "enable_compression" => {
                    cfg.enable_compression = parse_bool(value)?;
                }
                "tck_mode" => {
                    cfg.tck_mode = parse_bool(value)?;
                }
                "use_odirect" => {
                    cfg.use_odirect = parse_bool(value)?;
                }
                "tls_enabled" => {
                    cfg.tls_enabled = parse_bool(value)?;
                }
                "tls_cert_path" => {
                    cfg.tls_cert_path = Some(PathBuf::from(parse_string(value)?));
                }
                "tls_key_path" => {
                    cfg.tls_key_path = Some(PathBuf::from(parse_string(value)?));
                }
                _ => {
                    return Err(RGraphError::Argument(format!(
                        "unknown config key: {}",
                        key
                    ).into()));
                }
            }
        }
        // A configuration loaded from TOML must be fully validated; an
        // invalid file is rejected with a typed error rather than silently
        // accepted.
        cfg.validate()?;
        Ok(cfg)
    }

    /// Merge environment variables into this config.
    ///
    /// Variables are expected to be named `<prefix>_<UPPER_SNAKE_KEY>`,
    /// e.g. `RGRAPH_MAX_CONNECTIONS=512`.
    pub fn merge_env(&mut self, prefix: &str) {
        let prefix = format!("{}_", prefix.to_ascii_uppercase());
        if let Ok(v) = std::env::var(format!("{}DATABASE_PATH", prefix)) {
            self.database_path = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var(format!("{}GRAPH_MODE", prefix))
            && let Ok(mode) = v.parse()
        {
            self.graph_mode = mode;
        }
        if let Ok(v) = std::env::var(format!("{}PAGE_CACHE_SIZE_MB", prefix))
            && let Ok(n) = v.parse()
        {
            self.page_cache_size_mb = n;
        }
        if let Ok(v) = std::env::var(format!("{}WAL_CACHE_SIZE_MB", prefix))
            && let Ok(n) = v.parse()
        {
            self.wal_cache_size_mb = n;
        }
        if let Ok(v) = std::env::var(format!("{}MAX_CONNECTIONS", prefix))
            && let Ok(n) = v.parse()
        {
            self.max_connections = n;
        }
        if let Ok(v) = std::env::var(format!("{}WORKER_THREADS", prefix))
            && let Ok(n) = v.parse()
        {
            self.worker_threads = n;
        }
        if let Ok(v) = std::env::var(format!("{}CPU_POOL_THREADS", prefix))
            && let Ok(n) = v.parse()
        {
            self.cpu_pool_threads = n;
        }
        if let Ok(v) = std::env::var(format!("{}DEFAULT_PAGE_SIZE", prefix))
            && let Ok(n) = v.parse()
        {
            self.default_page_size = n;
        }
        if let Ok(v) = std::env::var(format!("{}ENABLE_COMPRESSION", prefix))
            && let Ok(b) = v.parse()
        {
            self.enable_compression = b;
        }
        if let Ok(v) = std::env::var(format!("{}TCK_MODE", prefix))
            && let Ok(b) = v.parse()
        {
            self.tck_mode = b;
        }
        if let Ok(v) = std::env::var(format!("{}USE_ODIRECT", prefix))
            && let Ok(b) = v.parse()
        {
            self.use_odirect = b;
        }
        if let Ok(v) = std::env::var(format!("{}TLS_ENABLED", prefix))
            && let Ok(b) = v.parse()
        {
            self.tls_enabled = b;
        }
        if let Ok(v) = std::env::var(format!("{}TLS_CERT_PATH", prefix)) {
            self.tls_cert_path = Some(PathBuf::from(v));
        }
        if let Ok(v) = std::env::var(format!("{}TLS_KEY_PATH", prefix)) {
            self.tls_key_path = Some(PathBuf::from(v));
        }
    }

    /// Validate every field, returning the first violation as a typed
    /// [`RGraphError::Argument`].
    ///
    /// This is the single source of truth for configuration validity and
    /// is invoked automatically by [`Config::from_toml_str`] (and therefore
    /// [`Config::from_toml`]) and by [`GraphBuilder::config`].
    ///
    /// # Errors
    ///
    /// Returns [`RGraphError::Argument`] when:
    ///
    /// - `database_path` is empty;
    /// - `page_cache_size_mb` or `wal_cache_size_mb` is `0` or exceeds
    ///   [`MAX_CACHE_SIZE_MB`];
    /// - `max_connections` is `0` or exceeds [`MAX_CONNECTIONS_LIMIT`];
    /// - `worker_threads` or `cpu_pool_threads` exceeds [`MAX_THREADS`]
    ///   (`0` is accepted and means "derive from CPU count");
    /// - `default_page_size` is not a power of two equal to the storage
    ///   engine's fixed [`PAGE_SIZE`](crate::storage::page::PAGE_SIZE);
    /// - `tls_enabled` is `true` but a certificate or key path is missing.
    pub fn validate(&self) -> Result<()> {
        // --- paths ---------------------------------------------------
        if self.database_path.as_os_str().is_empty() {
            return Err(RGraphError::Argument(
                "database_path must not be empty".into(),
            ));
        }

        // --- cache sizes ---------------------------------------------
        if self.page_cache_size_mb == 0 {
            return Err(RGraphError::Argument(
                "page_cache_size_mb must be > 0".into(),
            ));
        }
        if self.page_cache_size_mb > MAX_CACHE_SIZE_MB {
            return Err(RGraphError::Argument(
                format!(
                    "page_cache_size_mb ({}) exceeds the maximum of {} MB",
                    self.page_cache_size_mb, MAX_CACHE_SIZE_MB
                )
                .into(),
            ));
        }
        if self.wal_cache_size_mb == 0 {
            return Err(RGraphError::Argument(
                "wal_cache_size_mb must be > 0".into(),
            ));
        }
        if self.wal_cache_size_mb > MAX_CACHE_SIZE_MB {
            return Err(RGraphError::Argument(
                format!(
                    "wal_cache_size_mb ({}) exceeds the maximum of {} MB",
                    self.wal_cache_size_mb, MAX_CACHE_SIZE_MB
                )
                .into(),
            ));
        }

        // --- connection / thread bounds ------------------------------
        if self.max_connections == 0 {
            return Err(RGraphError::Argument(
                "max_connections must be > 0".into(),
            ));
        }
        if self.max_connections > MAX_CONNECTIONS_LIMIT {
            return Err(RGraphError::Argument(
                format!(
                    "max_connections ({}) exceeds the maximum of {}",
                    self.max_connections, MAX_CONNECTIONS_LIMIT
                )
                .into(),
            ));
        }
        if self.worker_threads > MAX_THREADS {
            return Err(RGraphError::Argument(
                format!(
                    "worker_threads ({}) exceeds the maximum of {}",
                    self.worker_threads, MAX_THREADS
                )
                .into(),
            ));
        }
        if self.cpu_pool_threads > MAX_THREADS {
            return Err(RGraphError::Argument(
                format!(
                    "cpu_pool_threads ({}) exceeds the maximum of {}",
                    self.cpu_pool_threads, MAX_THREADS
                )
                .into(),
            ));
        }

        // --- page size ------------------------------------------------
        if !self.default_page_size.is_power_of_two() {
            return Err(RGraphError::Argument(
                format!(
                    "default_page_size ({}) must be a power of two",
                    self.default_page_size
                )
                .into(),
            ));
        }
        if self.default_page_size != crate::storage::page::PAGE_SIZE {
            return Err(RGraphError::Argument(
                format!(
                    "default_page_size ({}) must equal the storage page size ({})",
                    self.default_page_size,
                    crate::storage::page::PAGE_SIZE
                )
                .into(),
            ));
        }

        // --- TLS consistency -----------------------------------------
        if self.tls_enabled {
            if self.tls_cert_path.is_none() {
                return Err(RGraphError::Argument(
                    "tls_enabled is true but tls_cert_path is not set".into(),
                ));
            }
            if self.tls_key_path.is_none() {
                return Err(RGraphError::Argument(
                    "tls_enabled is true but tls_key_path is not set".into(),
                ));
            }
        }

        Ok(())
    }
}

fn parse_string(value: &str) -> Result<String> {
    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        Ok(value[1..value.len() - 1].to_string())
    } else {
        Err(RGraphError::Argument(format!(
            "expected quoted string, got {}",
            value
        ).into()))
    }
}

fn parse_usize(value: &str) -> Result<usize> {
    value
        .parse()
        .map_err(|e| RGraphError::Argument(format!("expected positive integer: {} ({:?})", value, e).into()))
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" | "on" => Ok(true),
        "false" | "no" | "0" | "off" => Ok(false),
        _ => Err(RGraphError::Argument(format!(
            "expected boolean, got {}",
            value
        ).into())),
    }
}

/// Consuming builder that constructs a [`Config`] and then opens a
/// [`Database`](crate::db::database::Database).
///
/// # Example
///
/// ```ignore
/// let db = GraphBuilder::new()
///     .path("/tmp/mydb")
///     .mode(GraphMode::Lpg)
///     .page_cache_size_mb(512)
///     .build()?;
/// ```
#[derive(Debug, Clone, Default)]
pub struct GraphBuilder {
    database_path: Option<PathBuf>,
    graph_mode: Option<GraphMode>,
    page_cache_size_mb: Option<usize>,
    wal_cache_size_mb: Option<usize>,
    max_connections: Option<usize>,
    worker_threads: Option<usize>,
    cpu_pool_threads: Option<usize>,
    default_page_size: Option<usize>,
    enable_compression: Option<bool>,
    tck_mode: Option<bool>,
    use_odirect: Option<bool>,
    tls_enabled: Option<bool>,
    tls_cert_path: Option<PathBuf>,
    tls_key_path: Option<PathBuf>,
    env_prefix: Option<String>,
    config_file: Option<PathBuf>,
}

impl GraphBuilder {
    /// Start with the default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the database directory path.
    pub fn path(mut self, path: impl Into<PathBuf>) -> Self {
        self.database_path = Some(path.into());
        self
    }

    /// Set the graph data model.
    pub fn mode(mut self, mode: GraphMode) -> Self {
        self.graph_mode = Some(mode);
        self
    }

    /// Set the page cache size in megabytes.
    pub fn page_cache_size_mb(mut self, size: usize) -> Self {
        self.page_cache_size_mb = Some(size);
        self
    }

    /// Set the WAL cache size in megabytes.
    pub fn wal_cache_size_mb(mut self, size: usize) -> Self {
        self.wal_cache_size_mb = Some(size);
        self
    }

    /// Set the maximum number of concurrent connections.
    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = Some(max);
        self
    }

    /// Set the number of async worker threads (0 = num_cpus).
    pub fn worker_threads(mut self, n: usize) -> Self {
        self.worker_threads = Some(n);
        self
    }

    /// Set the number of CPU-bound rayon threads (0 = num_cpus).
    pub fn cpu_pool_threads(mut self, n: usize) -> Self {
        self.cpu_pool_threads = Some(n);
        self
    }

    /// Set the default page size in bytes.
    pub fn default_page_size(mut self, size: usize) -> Self {
        self.default_page_size = Some(size);
        self
    }

    /// Enable or disable page compression.
    pub fn enable_compression(mut self, enabled: bool) -> Self {
        self.enable_compression = Some(enabled);
        self
    }

    /// Enable or disable TCK strict mode.
    pub fn tck_mode(mut self, enabled: bool) -> Self {
        self.tck_mode = Some(enabled);
        self
    }

    /// Enable or disable O_DIRECT for data file I/O.
    pub fn use_odirect(mut self, enabled: bool) -> Self {
        self.use_odirect = Some(enabled);
        self
    }

    /// Enable or disable TLS for the server listener.
    ///
    /// When enabling TLS, also set [`GraphBuilder::tls_cert_path`] and
    /// [`GraphBuilder::tls_key_path`]; otherwise [`GraphBuilder::config`]
    /// rejects the configuration.
    pub fn tls_enabled(mut self, enabled: bool) -> Self {
        self.tls_enabled = Some(enabled);
        self
    }

    /// Set the TLS certificate chain path (PEM).
    pub fn tls_cert_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.tls_cert_path = Some(path.into());
        self
    }

    /// Set the TLS private key path (PEM).
    pub fn tls_key_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.tls_key_path = Some(path.into());
        self
    }

    /// Load configuration from a TOML file before applying builder overrides.
    ///
    /// The file is read at `build()` time, not immediately.
    pub fn config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }

    /// Merge environment variables with the given prefix before applying
    /// builder overrides.
    ///
    /// The prefix is stored and evaluated at `build()` time.
    pub fn env_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.env_prefix = Some(prefix.into());
        self
    }

    /// Consume the builder and return the fully resolved [`Config`].
    ///
    /// Resolution order (later overrides earlier):
    /// 1. Defaults
    /// 2. TOML file (if set)
    /// 3. Environment variables (if prefix set)
    /// 4. Builder method overrides
    pub fn config(self) -> Result<Config> {
        let mut cfg = Config::default();

        // Layer 2: TOML file.
        if let Some(path) = self.config_file {
            cfg = Config::from_toml(path)?;
        }

        // Layer 3: environment variables.
        if let Some(prefix) = self.env_prefix {
            cfg.merge_env(&prefix);
        }

        // Layer 4: builder overrides win.
        if let Some(v) = self.database_path { cfg.database_path = v; }
        if let Some(v) = self.graph_mode { cfg.graph_mode = v; }
        if let Some(v) = self.page_cache_size_mb { cfg.page_cache_size_mb = v; }
        if let Some(v) = self.wal_cache_size_mb { cfg.wal_cache_size_mb = v; }
        if let Some(v) = self.max_connections { cfg.max_connections = v; }
        if let Some(v) = self.worker_threads { cfg.worker_threads = v; }
        if let Some(v) = self.cpu_pool_threads { cfg.cpu_pool_threads = v; }
        if let Some(v) = self.default_page_size { cfg.default_page_size = v; }
        if let Some(v) = self.enable_compression { cfg.enable_compression = v; }
        if let Some(v) = self.tck_mode { cfg.tck_mode = v; }
        if let Some(v) = self.use_odirect { cfg.use_odirect = v; }
        if let Some(v) = self.tls_enabled { cfg.tls_enabled = v; }
        if let Some(v) = self.tls_cert_path { cfg.tls_cert_path = Some(v); }
        if let Some(v) = self.tls_key_path { cfg.tls_key_path = Some(v); }

        // The fully resolved configuration is validated once, after every
        // layer has been applied.
        cfg.validate()?;
        Ok(cfg)
    }

    /// Consume the builder and open a [`Database`](crate::db::database::Database).
    ///
    /// If the directory does not exist it is created.  If the database
    /// does not yet exist it is initialised; otherwise it is opened.
    ///
    /// The [`GraphMode`] chosen via [`GraphBuilder::mode`] is threaded into
    /// the returned handle so the query layer can dispatch correctly.
    pub fn build(self) -> Result<crate::db::database::Database> {
        let cfg = self.config()?;
        let fs = crate::io::posix::PosixFileSystem::new(cfg.use_odirect);
        let mode = cfg.graph_mode;
        if cfg.database_path.exists() {
            crate::db::database::Database::open(&cfg.database_path, &fs, mode)
                .map_err(|e| RGraphError::Io(format!("failed to open database: {}", e).into()))
        } else {
            crate::db::database::Database::init(&cfg.database_path, &fs, mode)
                .map_err(|e| RGraphError::Io(format!("failed to init database: {}", e).into()))
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.graph_mode, GraphMode::Lpg);
        assert_eq!(cfg.page_cache_size_mb, 256);
        assert_eq!(cfg.max_connections, 1024);
        assert!(cfg.enable_compression);
        assert!(!cfg.tck_mode);
    }

    #[test]
    fn builder_overrides() {
        let cfg = GraphBuilder::new()
            .path("/tmp/testdb")
            .mode(GraphMode::Rdf)
            .page_cache_size_mb(512)
            .max_connections(2048)
            .tck_mode(true)
            .config()
            .unwrap();

        assert_eq!(cfg.database_path, PathBuf::from("/tmp/testdb"));
        assert_eq!(cfg.graph_mode, GraphMode::Rdf);
        assert_eq!(cfg.page_cache_size_mb, 512);
        assert_eq!(cfg.max_connections, 2048);
        assert!(cfg.tck_mode);
    }

    #[test]
    fn builder_validation_rejects_zero_cache() {
        let result = GraphBuilder::new().page_cache_size_mb(0).config();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("page_cache_size_mb"));
    }

    #[test]
    fn toml_parsing() {
        let text = r#"
            database_path = "/var/lib/rgraph"
            graph_mode = "RDF"
            page_cache_size_mb = 128
            enable_compression = false
            tck_mode = true
        "#;
        let cfg = Config::from_toml_str(text).unwrap();
        assert_eq!(cfg.database_path, PathBuf::from("/var/lib/rgraph"));
        assert_eq!(cfg.graph_mode, GraphMode::Rdf);
        assert_eq!(cfg.page_cache_size_mb, 128);
        assert!(!cfg.enable_compression);
        assert!(cfg.tck_mode);
    }

    #[test]
    fn toml_unknown_key_errors() {
        let result = Config::from_toml_str("unknown_key = 42");
        assert!(result.is_err());
    }

    #[test]
    fn env_merge() {
        // Set a few env vars, merge, then clear them.
        unsafe {
            std::env::set_var("RGRAPH_TEST_MAX_CONNECTIONS", "512");
            std::env::set_var("RGRAPH_TEST_PAGE_CACHE_SIZE_MB", "64");
            std::env::set_var("RGRAPH_TEST_TCK_MODE", "true");
        }

        let mut cfg = Config::default();
        cfg.merge_env("RGRAPH_TEST");

        assert_eq!(cfg.max_connections, 512);
        assert_eq!(cfg.page_cache_size_mb, 64);
        assert!(cfg.tck_mode);

        unsafe {
            std::env::remove_var("RGRAPH_TEST_MAX_CONNECTIONS");
            std::env::remove_var("RGRAPH_TEST_PAGE_CACHE_SIZE_MB");
            std::env::remove_var("RGRAPH_TEST_TCK_MODE");
        }
    }

    #[test]
    fn graph_mode_parsing() {
        assert_eq!("LPG".parse::<GraphMode>().unwrap(), GraphMode::Lpg);
        assert_eq!("RDF".parse::<GraphMode>().unwrap(), GraphMode::Rdf);
        assert!("invalid".parse::<GraphMode>().is_err());
    }

    #[test]
    fn builder_layering_toml_then_override() {
        // Create a temporary TOML file.
        let dir = std::env::temp_dir().join("rgraph_test_config");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("rgraph.toml");
        std::fs::write(
            &file_path,
            r#"database_path = "/from/toml"
page_cache_size_mb = 64"#,
        )
        .unwrap();

        let cfg = GraphBuilder::new()
            .config_file(&file_path)
            .path("/from/builder")
            .config()
            .unwrap();

        // Builder override wins over TOML.
        assert_eq!(cfg.database_path, PathBuf::from("/from/builder"));
        assert_eq!(cfg.page_cache_size_mb, 64);

        let _ = std::fs::remove_file(&file_path);
        let _ = std::fs::remove_dir(&dir);
    }

    // ----------------------------------------------------------------
    // Task 186: configuration validation hardening
    // ----------------------------------------------------------------

    #[test]
    fn default_config_is_valid() {
        // `Config::default()` must always pass validation; it is the base
        // layer every other configuration is built upon.
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn default_page_size_matches_storage_page_size() {
        assert_eq!(
            Config::default().default_page_size,
            crate::storage::page::PAGE_SIZE
        );
    }

    #[test]
    fn from_toml_str_runs_validation() {
        // A non-power-of-two page size must be rejected by from_toml_str.
        let text = "default_page_size = 5000";
        let result = Config::from_toml_str(text);
        assert!(result.is_err(), "non-power-of-two page size must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("default_page_size"), "got: {msg}");
    }

    #[test]
    fn non_power_of_two_page_size_rejected() {
        let mut cfg = Config::default();
        cfg.default_page_size = 5000; // not a power of two
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("power of two"), "got: {err}");
    }

    #[test]
    fn power_of_two_but_wrong_page_size_rejected() {
        let mut cfg = Config::default();
        cfg.default_page_size = 4096; // power of two, but != storage PAGE_SIZE (8192)
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("storage page size"),
            "got: {err}"
        );
    }

    #[test]
    fn correct_page_size_accepted() {
        let mut cfg = Config::default();
        cfg.default_page_size = crate::storage::page::PAGE_SIZE;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn empty_database_path_rejected() {
        let mut cfg = Config::default();
        cfg.database_path = PathBuf::new();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("database_path"), "got: {err}");
    }

    #[test]
    fn zero_max_connections_rejected() {
        let mut cfg = Config::default();
        cfg.max_connections = 0;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("max_connections"), "got: {err}");
    }

    #[test]
    fn absurd_cache_size_rejected() {
        let mut cfg = Config::default();
        cfg.page_cache_size_mb = MAX_CACHE_SIZE_MB + 1;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("page_cache_size_mb"), "got: {err}");
    }

    #[test]
    fn absurd_thread_count_rejected() {
        let mut cfg = Config::default();
        cfg.worker_threads = MAX_THREADS + 1;
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("worker_threads"), "got: {err}");
    }

    #[test]
    fn zero_threads_means_auto_and_is_accepted() {
        let mut cfg = Config::default();
        cfg.worker_threads = 0;
        cfg.cpu_pool_threads = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn tls_enabled_without_cert_rejected() {
        let mut cfg = Config::default();
        cfg.tls_enabled = true;
        cfg.tls_key_path = Some(PathBuf::from("/etc/rgraph/key.pem"));
        // cert missing
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("tls_cert_path"), "got: {err}");
    }

    #[test]
    fn tls_enabled_without_key_rejected() {
        let mut cfg = Config::default();
        cfg.tls_enabled = true;
        cfg.tls_cert_path = Some(PathBuf::from("/etc/rgraph/cert.pem"));
        // key missing
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("tls_key_path"), "got: {err}");
    }

    #[test]
    fn tls_enabled_with_both_paths_accepted() {
        let mut cfg = Config::default();
        cfg.tls_enabled = true;
        cfg.tls_cert_path = Some(PathBuf::from("/etc/rgraph/cert.pem"));
        cfg.tls_key_path = Some(PathBuf::from("/etc/rgraph/key.pem"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn from_toml_str_rejects_tls_without_cert() {
        let text = r#"
            tls_enabled = true
            tls_key_path = "/etc/rgraph/key.pem"
        "#;
        let err = Config::from_toml_str(text).unwrap_err();
        assert!(err.to_string().contains("tls_cert_path"), "got: {err}");
    }

    #[test]
    fn from_toml_str_accepts_valid_tls() {
        let text = r#"
            tls_enabled = true
            tls_cert_path = "/etc/rgraph/cert.pem"
            tls_key_path = "/etc/rgraph/key.pem"
        "#;
        let cfg = Config::from_toml_str(text).unwrap();
        assert!(cfg.tls_enabled);
        assert_eq!(
            cfg.tls_cert_path,
            Some(PathBuf::from("/etc/rgraph/cert.pem"))
        );
        assert_eq!(cfg.tls_key_path, Some(PathBuf::from("/etc/rgraph/key.pem")));
    }

    #[test]
    fn valid_toml_round_trips_through_validation() {
        // A fully specified, valid TOML must parse and validate cleanly.
        let text = format!(
            r#"
                database_path = "/var/lib/rgraph"
                graph_mode = "LPG"
                page_cache_size_mb = 256
                wal_cache_size_mb = 64
                max_connections = 1024
                worker_threads = 4
                cpu_pool_threads = 4
                default_page_size = {}
                enable_compression = true
                tck_mode = false
                use_odirect = false
            "#,
            crate::storage::page::PAGE_SIZE
        );
        let cfg = Config::from_toml_str(&text).expect("valid TOML must round-trip");
        assert_eq!(cfg.database_path, PathBuf::from("/var/lib/rgraph"));
        assert_eq!(cfg.default_page_size, crate::storage::page::PAGE_SIZE);
        assert_eq!(cfg.worker_threads, 4);
        // Re-validating the parsed config is idempotent.
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn from_toml_str_rejects_zero_cache() {
        // from_toml_str now enforces full validation, not just parsing.
        let err = Config::from_toml_str("page_cache_size_mb = 0").unwrap_err();
        assert!(err.to_string().contains("page_cache_size_mb"), "got: {err}");
    }

    #[test]
    fn validation_error_is_argument_class() {
        use crate::error::{ErrorRegistry, TckErrorClass};
        let mut cfg = Config::default();
        cfg.max_connections = 0;
        let err = cfg.validate().unwrap_err();
        // Configuration problems are caller-supplied argument errors.
        assert_eq!(ErrorRegistry::class(&err), TckErrorClass::ArgumentError);
    }
}
