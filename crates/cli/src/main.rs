//! Command-line entry point.
//!
//! `javelin serve` is what an editor launches. The other subcommands exist so
//! the engine can be exercised without an editor in the loop, which is how most
//! debugging and all benchmarking starts.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use javelin_core::config::{Config, LogLevel};

#[derive(Debug, Parser)]
#[command(name = "javelin", version, about = "A fast Java language server", long_about = None)]
struct Cli {
    /// Log verbosity. Overridden by the JAVELIN_LOG environment variable.
    #[arg(long, global = true, value_enum, default_value = "info")]
    log_level: LogLevelArg,

    /// Also write structured logs to this file.
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve LSP over stdin/stdout. This is what editors run.
    Serve,
    /// Report the environment Javelin sees: cache location, detected build
    /// system, thread budget.
    Doctor {
        /// Workspace to inspect. Defaults to the current directory.
        #[arg(default_value = ".")]
        workspace: PathBuf,
    },
    /// Parse a Java file and report the result. Useful for checking whether a
    /// file the grammar chokes on is the reason a feature looks broken.
    Parse { file: PathBuf },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum LogLevelArg {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<LogLevelArg> for LogLevel {
    fn from(value: LogLevelArg) -> Self {
        match value {
            LogLevelArg::Error => LogLevel::Error,
            LogLevelArg::Warn => LogLevel::Warn,
            LogLevelArg::Info => LogLevel::Info,
            LogLevelArg::Debug => LogLevel::Debug,
            LogLevelArg::Trace => LogLevel::Trace,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = Config::default();
    config.logging.level = cli.log_level.into();
    config.logging.file = cli.log_file.clone();
    javelin_core::telemetry::init(&config.logging).context("failed to set up logging")?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                "starting language server"
            );
            javelin_server::run_stdio(config).context("language server exited with an error")
        }
        Command::Doctor { workspace } => doctor(&config, &workspace),
        Command::Parse { file } => parse(&file),
    }
}

fn doctor(config: &Config, workspace: &std::path::Path) -> Result<()> {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let cache_root = javelin_core::paths::cache_root(config.index.cache_dir.as_deref())
        .context("could not resolve a cache directory")?;

    println!("javelin {}", env!("CARGO_PKG_VERSION"));
    println!("workspace:        {}", workspace.display());
    println!("cache root:       {}", cache_root.display());
    println!(
        "workspace cache:  {}",
        javelin_core::paths::workspace_cache_dir(&cache_root, &workspace).display()
    );
    println!(
        "build system:     {}",
        javelin_build::detect(&workspace).map_or("none detected", |kind| kind.name())
    );
    println!(
        "background threads: {}",
        config.workers.resolved_background_threads()
    );
    // What the last session's build tool answered, without running it:
    // running it is the background job's, and costs minutes.
    let classpath =
        javelin_core::paths::workspace_cache_dir(&cache_root, &workspace).join("classpath.txt");
    println!(
        "dependency jars:  {}",
        std::fs::read_to_string(&classpath).map_or_else(
            |_| "none resolved yet".to_owned(),
            |text| format!(
                "{} (as last resolved)",
                text.lines().count().saturating_sub(1)
            )
        )
    );
    report_classpath(config);
    report_workspace(config, &workspace);
    Ok(())
}

/// The JDK the server would read in the background, and what is cached for it.
///
/// This is the first thing to look at when a JDK name does not resolve: no
/// home means discovery found nothing, no `src.zip` means there are no sources
/// to read type facts from, and no cached facts means the next session pays
/// for the read again.
fn report_classpath(config: &Config) {
    let report = javelin_server::classpath::report(config);
    match &report.jdk {
        Some(jdk) => {
            println!("jdk home:         {}", jdk.home.display());
            println!(
                "jdk version:      {}",
                jdk.version
                    .map_or_else(|| "unknown".to_owned(), |v| v.to_string())
            );
            println!(
                "jdk sources:      {}",
                jdk.sources.as_ref().map_or_else(
                    || "none (no lib/src.zip)".to_owned(),
                    |p| p.display().to_string()
                )
            );
        }
        None => println!("jdk home:         none detected"),
    }
    println!(
        "classpath cache:  {}",
        report
            .cache
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |p| p.display().to_string())
    );
    println!(
        "cached facts:     {}",
        report
            .cached_facts
            .map_or_else(|| "none for this jdk".to_owned(), |count| count.to_string())
    );
}

/// The workspace's own source index, and whether a previous session left one.
///
/// No counts means the next session walks every file again — which is correct
/// but slower, and the first thing to look at when a cross-file answer is
/// missing right after startup.
fn report_workspace(config: &Config, workspace: &std::path::Path) {
    let report = javelin_server::workspace::report(config, workspace);
    println!(
        "source index:     {}",
        report
            .cache
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |p| p.display().to_string())
    );
    println!(
        "indexed sources:  {}",
        report.indexed.map_or_else(
            || "none for this workspace".to_owned(),
            |(files, types)| format!("{files} files, {types} types")
        )
    );
}

fn parse(file: &std::path::Path) -> Result<()> {
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("could not read {}", file.display()))?;
    let document = javelin_document::TextDocument::new(
        javelin_document::DocumentId::new(file.to_string_lossy()),
        0,
        text,
    );

    let mut parser =
        javelin_parser::JavaParser::new().context("could not load the Java grammar")?;
    let started = std::time::Instant::now();
    let tree = parser.parse(&document).context("parse failed")?;
    let elapsed = started.elapsed();

    println!("{}", file.display());
    println!("  bytes:    {}", document.text().len());
    println!("  lines:    {}", document.line_index().line_count());
    println!("  parsed in {:.3} ms", elapsed.as_secs_f64() * 1000.0);
    println!("  recovered from syntax errors: {}", tree.has_errors());
    Ok(())
}
