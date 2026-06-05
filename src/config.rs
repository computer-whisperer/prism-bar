//! KDL config — `~/.config/prism-bar/config.kdl`.
//!
//! Same stack and style as prism's own config (knuffel-decoded KDL,
//! miette diagnostics). A missing file means defaults; a file that
//! fails to parse is a hard error with a source-annotated report —
//! silently falling back to defaults would mask typos.
//!
//! ```kdl
//! // Which outputs get a bar (connector names). No output nodes → all.
//! output "DP-1"
//! output "HDMI-A-1"
//!
//! height 40
//! margin 6
//! position "top"   // "top" | "bottom"
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, knuffel::Decode)]
pub struct Config {
    /// Outputs to put a bar on. Empty = every output.
    #[knuffel(children(name = "output"))]
    pub outputs: Vec<OutputConfig>,
    /// Bar height in logical pixels.
    #[knuffel(child, unwrap(argument), default = 40)]
    pub height: u32,
    /// Floating margin off the screen edges, logical pixels.
    #[knuffel(child, unwrap(argument), default = 6)]
    pub margin: i32,
    /// Screen edge the bar docks to.
    #[knuffel(child, unwrap(argument, str), default)]
    pub position: Position,
}

#[derive(Debug, knuffel::Decode)]
pub struct OutputConfig {
    /// Connector name, e.g. `DP-1`.
    #[knuffel(argument)]
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Top,
    Bottom,
}

impl std::str::FromStr for Position {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            other => Err(format!("expected \"top\" or \"bottom\", got \"{other}\"")),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            outputs: Vec::new(),
            height: 40,
            margin: 6,
            position: Position::Top,
        }
    }
}

impl Config {
    /// Whether this config wants a bar on the named output.
    pub fn wants_output(&self, name: &str) -> bool {
        self.outputs.is_empty() || self.outputs.iter().any(|o| o.name == name)
    }

    /// `$PRISM_BAR_CONFIG`, else `$XDG_CONFIG_HOME/prism-bar/config.kdl`,
    /// else `~/.config/prism-bar/config.kdl`.
    pub fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("PRISM_BAR_CONFIG") {
            return Some(PathBuf::from(p));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("prism-bar").join("config.kdl"))
    }

    pub fn load() -> Result<Self> {
        let Some(path) = Self::path() else {
            tracing::warn!("no config path resolvable (no $HOME); using defaults");
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no config at {}; using defaults", path.display());
                return Ok(Self::default());
            }
            Err(err) => {
                return Err(err).context(format!("reading {}", path.display()));
            }
        };
        match knuffel::parse::<Config>(&path.to_string_lossy(), &text) {
            Ok(config) => Ok(config),
            Err(err) => {
                // miette's fancy renderer points at the offending span.
                anyhow::bail!("config error:\n{:?}", miette::Report::new(err));
            }
        }
    }
}
