//! Zed integration.
//!
//! The extension's entire job is to find the `javelin` binary, tell Zed how
//! to launch it, and hand it the settings the user wrote for it. Syntax
//! highlighting comes from Zed's Java extension, and every piece of Java
//! intelligence comes from the server over LSP. Nothing language-specific
//! belongs in this file.
//!
//! Installed as a dev extension from this directory; not published.

use zed_extension_api::settings::LspSettings;
use zed_extension_api::{self as zed, Command, LanguageServerId, Result, Worktree};

/// The key under `lsp` in Zed's settings, and the id in `extension.toml`.
const SERVER: &str = "javelin";

struct JavelinExtension;

impl JavelinExtension {
    /// Resolve the server binary, its arguments and any environment the user
    /// set for it.
    ///
    /// An explicit setting wins, so a contributor can point Zed at
    /// `target/release/javelin` without installing anything. Otherwise fall back
    /// to `PATH`.
    fn command(worktree: &Worktree) -> Result<Command> {
        let configured = LspSettings::for_worktree(SERVER, worktree)
            .ok()
            .and_then(|settings| settings.binary);

        let args = configured
            .as_ref()
            .and_then(|binary| binary.arguments.clone())
            .unwrap_or_else(|| vec!["serve".to_owned()]);
        let mut env = worktree.shell_env();
        if let Some(extra) = configured.as_ref().and_then(|binary| binary.env.clone()) {
            env.extend(extra);
        }

        let command = match configured.and_then(|binary| binary.path) {
            Some(path) => path,
            None => worktree.which(SERVER).ok_or_else(|| {
                "javelin is not on PATH. Build it with `cargo build --release` and set \
                 lsp.javelin.binary.path in your Zed settings, or put it on PATH."
                    .to_owned()
            })?,
        };
        Ok(Command { command, args, env })
    }
}

impl zed::Extension for JavelinExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        Self::command(worktree)
    }

    /// `lsp.javelin.initialization_options`, passed through untouched: the
    /// server reads every setting it has — `build.importOnStartup` among them —
    /// from `initializationOptions`.
    fn language_server_initialization_options(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        Ok(LspSettings::for_worktree(SERVER, worktree)
            .ok()
            .and_then(|settings| settings.initialization_options))
    }
}

zed::register_extension!(JavelinExtension);
