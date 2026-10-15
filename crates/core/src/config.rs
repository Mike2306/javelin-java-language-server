//! The single owner of user-tunable settings.
//!
//! Every subsystem reads its knobs from here instead of consulting environment
//! variables or globals on its own. Most fields are placeholders for behaviour
//! that does not exist yet; they are declared now so the settings surface stays
//! in one file as features land.
//!
//! Settings arrive from the editor as an arbitrary JSON blob. Parsing is
//! forgiving on purpose: an unknown or malformed key downgrades to the default
//! and produces a warning rather than refusing to start.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_filter(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LoggingConfig {
    pub level: LogLevel,
    /// Mirror structured logs to a file. Stderr is always used; stdout belongs to
    /// the LSP transport and must never be written to.
    pub file: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            file: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IndexConfig {
    /// Where persistent indexes live. `None` resolves to the per-user cache
    /// directory at runtime; see [`crate::paths`].
    pub cache_dir: Option<PathBuf>,
    /// Index the workspace in the background after startup. Turning this off
    /// must still leave single-file features fully working.
    pub background_indexing: bool,
    /// Reuse an index left over from a previous session.
    pub persist: bool,
    /// Skip files above this size. Generated monsters are not worth the
    /// latency. Also bounds one decompressed `src.zip` entry when the
    /// classpath is read.
    pub max_file_size_bytes: u64,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            cache_dir: None,
            background_indexing: true,
            persist: true,
            max_file_size_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkerConfig {
    /// Threads available for background work. `None` derives it from the machine,
    /// leaving headroom so background work cannot starve interactive requests.
    pub background_threads: Option<usize>,
}

impl WorkerConfig {
    /// Resolved background thread count, always at least 1.
    ///
    /// The floor applies to an explicitly configured value too. A client can
    /// send `backgroundThreads: 0` through `initializationOptions`, and a pool
    /// with no workers does not degrade — it silently stops reparsing for the
    /// rest of the session.
    pub fn resolved_background_threads(&self) -> usize {
        self.background_threads
            .unwrap_or_else(|| {
                let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
                cpus.saturating_sub(1)
            })
            .max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DiagnosticsConfig {
    pub enabled: bool,
    /// Debounce before recomputing diagnostics for a changed document.
    pub debounce_ms: u64,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: 250,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JdkConfig {
    /// Explicit JDK home. When unset, JDK discovery applies.
    pub home: Option<PathBuf>,
    /// Additional roots to search for installed JDKs.
    pub search_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildSystem {
    #[default]
    Auto,
    Maven,
    Gradle,
    /// Ignore build files entirely and treat the workspace as loose sources.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BuildConfig {
    pub system: BuildSystem,
    /// Ask the build system for the dependency classpath in the background at
    /// startup. Never blocks the editor becoming usable.
    ///
    /// **Off by default, because it runs the project's code**: the wrapper
    /// script, the Gradle build script, the Maven plugins. Javelin is a tool
    /// for reading code that may not be trusted, and opening a checkout must
    /// not execute it unasked. Turn it on for a project you trust — ADR 0005.
    pub import_on_startup: bool,
    /// With the classpath imported, ask the build tool once per classpath for
    /// each dependency's `-sources.jar`, so a library member jumps, and hovers
    /// with its parameter names and Javadoc. Downloads from the build's
    /// repositories, in the background. Nothing is fetched without
    /// `import_on_startup`.
    pub download_sources: bool,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            system: BuildSystem::Auto,
            import_on_startup: false,
            download_sources: true,
        }
    }
}

/// An external formatter: Javelin has no opinion on layout, so it borrows the
/// project's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FormatConfig {
    /// The formatter's argv, reading the file on stdin and writing it
    /// formatted to stdout: `["google-java-format", "-"]`. Empty, the default,
    /// means no formatting is advertised.
    pub command: Vec<String>,
    /// How long one run may take before it is killed and the request fails.
    pub timeout_ms: u64,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            timeout_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub logging: LoggingConfig,
    pub index: IndexConfig,
    pub workers: WorkerConfig,
    pub diagnostics: DiagnosticsConfig,
    pub jdk: JdkConfig,
    pub build: BuildConfig,
    pub format: FormatConfig,
}

impl Config {
    /// Parse settings received from an editor.
    ///
    /// Returns the defaults plus a warning rather than an error when the blob is
    /// unusable: a typo in a settings file must not stop the server from
    /// starting.
    pub fn from_json(value: &serde_json::Value) -> (Config, Option<String>) {
        match serde_json::from_value::<Config>(value.clone()) {
            Ok(config) => (config, None),
            Err(err) => (
                Config::default(),
                Some(format!(
                    "ignoring invalid configuration, using defaults: {err}"
                )),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opening a checkout must not execute it: the build tool runs the
    /// project's code, so it is asked only when the user says so.
    #[test]
    fn the_build_tool_is_not_run_unless_asked() {
        assert!(!Config::default().build.import_on_startup);
    }

    #[test]
    fn empty_settings_yield_defaults() {
        let (config, warning) = Config::from_json(&serde_json::json!({}));
        assert_eq!(config, Config::default());
        assert!(warning.is_none());
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let (config, warning) =
            Config::from_json(&serde_json::json!({ "somethingFromTheFuture": 42 }));
        assert_eq!(config, Config::default());
        assert!(warning.is_none());
    }

    #[test]
    fn partial_settings_only_override_what_they_mention() {
        let (config, warning) =
            Config::from_json(&serde_json::json!({ "logging": { "level": "debug" } }));
        assert!(warning.is_none());
        assert_eq!(config.logging.level, LogLevel::Debug);
        assert_eq!(config.index, IndexConfig::default());
    }

    #[test]
    fn a_malformed_blob_degrades_to_defaults_with_a_warning() {
        let (config, warning) =
            Config::from_json(&serde_json::json!({ "logging": { "level": "shout" } }));
        assert_eq!(config, Config::default());
        assert!(warning.is_some());
    }

    #[test]
    fn background_threads_leave_headroom_but_never_reach_zero() {
        assert!(WorkerConfig::default().resolved_background_threads() >= 1);
        let pinned = WorkerConfig {
            background_threads: Some(4),
        };
        assert_eq!(pinned.resolved_background_threads(), 4);
        // Client-settable, and zero workers is not "less background work" —
        // it is no reparsing at all, for the rest of the session.
        let none_at_all = WorkerConfig {
            background_threads: Some(0),
        };
        assert_eq!(none_at_all.resolved_background_threads(), 1);
    }
}
