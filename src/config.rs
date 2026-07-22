//! Credential + settings loading.
//!
//! Resolution order:
//!   1. Environment variables `TCL_USERNAME` / `TCL_PASSWORD`
//!   2. Config file `~/.config/tclac/config.toml`
//!   3. Interactive prompt (and offer to save to the config file)
//!
//! Credentials are never hardcoded and never printed back.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub username: String,
    pub password: String,
    /// Optional: pin a specific device by its cloud id (thingName). If empty,
    /// the first device returned by the account is used.
    #[serde(default)]
    pub device_id: Option<String>,
}

fn config_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "tclac")
        .context("could not determine a config directory for this platform")?;
    Ok(dirs.config_dir().join("config.toml"))
}

impl Config {
    /// Load credentials from env, then file, then interactive prompt.
    pub fn load() -> Result<Config> {
        // 1. Environment
        if let (Ok(username), Ok(password)) =
            (std::env::var("TCL_USERNAME"), std::env::var("TCL_PASSWORD"))
        {
            if !username.is_empty() && !password.is_empty() {
                return Ok(Config {
                    username,
                    password,
                    device_id: std::env::var("TCL_DEVICE_ID")
                        .ok()
                        .filter(|s| !s.is_empty()),
                });
            }
        }

        // 2. Config file
        let path = config_path()?;
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let cfg: Config =
                toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
            if !cfg.username.is_empty() && !cfg.password.is_empty() {
                return Ok(cfg);
            }
        }

        // 3. Interactive prompt
        Self::prompt_interactive(&path)
    }

    fn prompt_interactive(path: &PathBuf) -> Result<Config> {
        eprintln!("No TCL Home credentials found.");
        eprintln!("(Set TCL_USERNAME / TCL_PASSWORD env vars to skip this prompt.)\n");

        let username = prompt_line("TCL Home email/username: ")?;
        let password = prompt_password("TCL Home password: ")?;

        let cfg = Config {
            username: username.trim().to_string(),
            password,
            device_id: None,
        };

        let save = prompt_line(&format!(
            "Save credentials to {} for next time? [y/N]: ",
            path.display()
        ))?;
        if save.trim().eq_ignore_ascii_case("y") {
            cfg.save(path)?;
            eprintln!("Saved. (chmod 600)\n");
        }
        Ok(cfg)
    }

    fn save(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(path, raw)?;
        // Best-effort tighten permissions on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

fn prompt_line(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim_end_matches(['\n', '\r']).to_string())
}

/// Read a password without echoing where possible (unix raw mode via crossterm).
fn prompt_password(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;

    // Use crossterm raw mode to suppress echo; fall back to plain read on error.
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    if enable_raw_mode().is_err() {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        return Ok(s.trim_end_matches(['\n', '\r']).to_string());
    }

    let mut password = String::new();
    use crossterm::event::{read, Event, KeyCode, KeyModifiers};
    loop {
        if let Event::Key(k) = read()? {
            match k.code {
                KeyCode::Enter => break,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    disable_raw_mode().ok();
                    anyhow::bail!("cancelled");
                }
                KeyCode::Backspace => {
                    password.pop();
                }
                KeyCode::Char(c) => password.push(c),
                _ => {}
            }
        }
    }
    disable_raw_mode().ok();
    eprintln!();
    Ok(password)
}
