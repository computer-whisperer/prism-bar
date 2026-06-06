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
//!
//! // Right-side modules in display order; presence enables. Omit the
//! // block for the default set: tray, cpu, memory, disk "/", clock.
//! modules {
//!     tray icon-size=22    // StatusNotifierItem system tray
//!     cpu hot=95           // hot= tints the gauge destructive at N%
//!     memory
//!     disk "/"             // repeatable, one gauge per path
//!     disk "/home"
//!     clock format="%H:%M" // chrono format string
//! }
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
    /// Panel background alpha, `0.0..=1.0`.
    #[knuffel(child, unwrap(argument), default = 0.8)]
    pub opacity: f32,
    /// Panel corner radius in logical pixels.
    #[knuffel(child, unwrap(argument), default = 12)]
    pub radius: u32,
    /// Color palette; see [`ThemeName`] for the vocabulary.
    #[knuffel(child, unwrap(argument, str), default)]
    pub theme: ThemeName,
    /// Whether the panel draws its border stroke.
    #[knuffel(child, unwrap(argument), default = true)]
    pub border: bool,
    /// System monitor poll cadence in seconds.
    #[knuffel(child, unwrap(argument), default = 2)]
    pub sample_interval: u64,
    /// Focused-window title truncation length (characters).
    #[knuffel(child, unwrap(argument), default = 80)]
    pub title_max_length: u32,
    /// Right-side modules in display order. None = default set.
    #[knuffel(child)]
    modules: Option<Modules>,
}

/// The eight stock damascene palettes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
    SlateBlueDark,
    SlateBlueLight,
    SandAmberDark,
    SandAmberLight,
    MauveVioletDark,
    MauveVioletLight,
}

impl std::str::FromStr for ThemeName {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "dark" => Self::Dark,
            "light" => Self::Light,
            "slate-blue-dark" => Self::SlateBlueDark,
            "slate-blue-light" => Self::SlateBlueLight,
            "sand-amber-dark" => Self::SandAmberDark,
            "sand-amber-light" => Self::SandAmberLight,
            "mauve-violet-dark" => Self::MauveVioletDark,
            "mauve-violet-light" => Self::MauveVioletLight,
            other => {
                return Err(format!(
                    "unknown theme \"{other}\"; expected one of: dark, light, \
                     slate-blue-dark, slate-blue-light, sand-amber-dark, \
                     sand-amber-light, mauve-violet-dark, mauve-violet-light"
                ));
            }
        })
    }
}

/// Appearance values shared with every BarApp.
#[derive(Debug, Clone, PartialEq)]
pub struct Appearance {
    pub opacity: f32,
    pub radius: f32,
    pub theme: ThemeName,
    pub border: bool,
    pub title_max_length: usize,
}

#[derive(Debug, knuffel::Decode)]
struct Modules {
    #[knuffel(children)]
    list: Vec<Module>,
}

/// One right-cluster module. Node name selects the variant.
#[derive(Debug, Clone, PartialEq, knuffel::Decode)]
pub enum Module {
    Tray(TrayOpts),
    Cpu(GaugeOpts),
    Memory(GaugeOpts),
    Disk(DiskOpts),
    Clock(ClockOpts),
}

#[derive(Debug, Clone, PartialEq, knuffel::Decode)]
pub struct GaugeOpts {
    /// Percentage at which the gauge tints destructive.
    #[knuffel(property, default = 90)]
    pub hot: u32,
    /// Bar length in logical pixels.
    #[knuffel(property, default = 84)]
    pub width: u32,
    /// Bar thickness in logical pixels.
    #[knuffel(property, default = 5)]
    pub thickness: u32,
}

#[derive(Debug, Clone, PartialEq, knuffel::Decode)]
pub struct DiskOpts {
    /// Mount point to monitor.
    #[knuffel(argument, default = String::from("/"))]
    pub path: String,
    #[knuffel(property, default = 90)]
    pub hot: u32,
    #[knuffel(property, default = 84)]
    pub width: u32,
    #[knuffel(property, default = 5)]
    pub thickness: u32,
}

#[derive(Debug, Clone, PartialEq, knuffel::Decode)]
pub struct TrayOpts {
    /// Icon edge length in logical pixels (KDL: `icon-size`).
    #[knuffel(property, default = 22)]
    pub icon_size: u32,
}

#[derive(Debug, Clone, PartialEq, knuffel::Decode)]
pub struct ClockOpts {
    /// chrono strftime format.
    #[knuffel(property, default = String::from("%H:%M:%S"))]
    pub format: String,
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
            opacity: 0.8,
            radius: 12,
            theme: ThemeName::Dark,
            border: true,
            sample_interval: 2,
            title_max_length: 80,
            modules: None,
        }
    }
}

impl Config {
    /// Whether this config wants a bar on the named output.
    pub fn wants_output(&self, name: &str) -> bool {
        self.outputs.is_empty() || self.outputs.iter().any(|o| o.name == name)
    }

    /// Right-cluster modules in display order.
    pub fn modules(&self) -> Vec<Module> {
        const GAUGE: GaugeOpts = GaugeOpts {
            hot: 90,
            width: 84,
            thickness: 5,
        };
        match &self.modules {
            Some(m) => m.list.clone(),
            None => vec![
                Module::Tray(TrayOpts { icon_size: 22 }),
                Module::Cpu(GAUGE),
                Module::Memory(GAUGE),
                Module::Disk(DiskOpts {
                    path: "/".into(),
                    hot: 90,
                    width: 84,
                    thickness: 5,
                }),
                Module::Clock(ClockOpts {
                    format: "%H:%M:%S".into(),
                }),
            ],
        }
    }

    /// Appearance values shared with every BarApp.
    pub fn appearance(&self) -> Appearance {
        Appearance {
            opacity: self.opacity,
            radius: self.radius as f32,
            theme: self.theme,
            border: self.border,
            title_max_length: self.title_max_length as usize,
        }
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
        let config = match knuffel::parse::<Config>(&path.to_string_lossy(), &text) {
            Ok(config) => config,
            Err(err) => {
                // miette's fancy renderer points at the offending span.
                anyhow::bail!("config error:\n{:?}", miette::Report::new(err));
            }
        };
        config.validate()?;
        Ok(config)
    }

    /// Checks knuffel can't express — currently: clock formats must
    /// actually format (a bad chrono specifier would otherwise panic
    /// at render time, once a second), and scalar ranges.
    fn validate(&self) -> Result<()> {
        use std::fmt::Write;
        for m in self.modules() {
            if let Module::Clock(c) = m {
                let mut s = String::new();
                if write!(s, "{}", chrono::Local::now().format(&c.format)).is_err() {
                    anyhow::bail!("invalid clock format string: {:?}", c.format);
                }
            }
        }
        if !(0.0..=1.0).contains(&self.opacity) {
            anyhow::bail!("opacity must be within 0.0..=1.0, got {}", self.opacity);
        }
        if self.sample_interval == 0 {
            anyhow::bail!("sample-interval must be at least 1 second");
        }
        Ok(())
    }
}
