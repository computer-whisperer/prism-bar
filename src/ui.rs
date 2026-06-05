//! The bar's damascene `App` — pure state → tree projection.
//!
//! The host pushes compositor state in via [`BarApp::set_state`] before
//! each build; clicks come back out through [`BarApp::take_activate`].
//! Modules degrade by absence: no workspaces → no pills, no focused
//! window → no title.

use std::rc::Rc;
use std::sync::LazyLock;

use damascene_core::prelude::*;
use damascene_core::SvgIcon;

use crate::config::Module;
use crate::sysmon::SysStats;
use crate::workspaces::WorkspaceView;

// Vendored lucide glyphs (ISC license) — the built-in icon set has no
// hardware vocabulary. `parse_current_color` makes them tint like the
// built-ins.
static ICON_CPU: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/cpu.svg")).expect("cpu.svg")
});
static ICON_MEM: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/memory-stick.svg"))
        .expect("memory-stick.svg")
});
static ICON_DISK: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(include_str!("../assets/icons/hard-drive.svg"))
        .expect("hard-drive.svg")
});

/// Longest title before middle-truncation; keeps the title from
/// crowding the clock until proper width-constrained truncation.
const TITLE_MAX_CHARS: usize = 80;

pub struct BarApp {
    /// Right-cluster modules in display order (from config).
    modules: Rc<Vec<Module>>,
    /// Formatted text per Clock module, in module order.
    clocks: Vec<String>,
    workspaces: Vec<WorkspaceView>,
    title: Option<String>,
    sys: SysStats,
    /// Workspace slot the user clicked, drained by the host.
    pending_activate: Option<usize>,
}

impl BarApp {
    pub fn new(modules: Rc<Vec<Module>>) -> Self {
        Self {
            modules,
            clocks: Vec::new(),
            workspaces: Vec::new(),
            title: None,
            sys: SysStats::default(),
            pending_activate: None,
        }
    }

    /// Host-side state push, called before each build.
    pub fn set_state(
        &mut self,
        workspaces: Vec<WorkspaceView>,
        title: Option<String>,
        sys: SysStats,
    ) {
        self.workspaces = workspaces;
        self.title = title;
        self.sys = sys;
    }

    /// Drain the workspace-switch request from the last event batch.
    pub fn take_activate(&mut self) -> Option<usize> {
        self.pending_activate.take()
    }
}

impl App for BarApp {
    fn before_build(&mut self) {
        let now = chrono::Local::now();
        self.clocks = self
            .modules
            .iter()
            .filter_map(|m| match m {
                // Formats are validated at config load.
                Module::Clock(c) => Some(now.format(&c.format).to_string()),
                _ => None,
            })
            .collect();
    }

    fn build(&self, cx: &BuildCx) -> El {
        let palette = cx.palette();

        let pills: Vec<El> = self
            .workspaces
            .iter()
            .filter(|ws| !ws.hidden)
            .map(|ws| {
                let b = button(ws.label.clone()).key(format!("ws-{}", ws.slot));
                if ws.active {
                    b.primary()
                } else if ws.urgent {
                    b.destructive()
                } else {
                    b.ghost()
                }
            })
            .collect();

        let title = self.title.as_deref().map(|t| {
            let t = middle_truncate(t, TITLE_MAX_CHARS);
            text(t).label().muted()
        });

        let mut items: Vec<El> = vec![text("prism").label().muted()];
        if !pills.is_empty() {
            items.push(row(pills).gap(tokens::SPACE_1));
        }
        if let Some(title) = title {
            items.push(title);
        }
        items.push(spacer());
        let mut clock_i = 0;
        for module in self.modules.iter() {
            match module {
                Module::Cpu(o) => {
                    if let Some(frac) = self.sys.cpu {
                        items.push(gauge_module(&ICON_CPU, frac, o.hot, palette, None));
                    }
                }
                Module::Memory(o) => {
                    if let Some(frac) = self.sys.mem {
                        items.push(gauge_module(&ICON_MEM, frac, o.hot, palette, None));
                    }
                }
                Module::Disk(o) => {
                    let frac = self
                        .sys
                        .disks
                        .iter()
                        .find(|(p, _)| p == &o.path)
                        .and_then(|(_, f)| *f);
                    if let Some(frac) = frac {
                        // Label non-root mounts so two disk gauges read.
                        let label = (o.path != "/").then_some(o.path.as_str());
                        items.push(gauge_module(&ICON_DISK, frac, o.hot, palette, label));
                    }
                }
                Module::Clock(_) => {
                    if let Some(clock) = self.clocks.get(clock_i) {
                        items.push(tabular(clock, *DIGIT_W_LABEL, &|s| text(s).label()));
                    }
                    clock_i += 1;
                }
            }
        }

        // The wl_surface is cleared transparent; the visible bar is this
        // rounded translucent panel, floated off the screen edge by the
        // layer-surface margins set in the host.
        row(items)
            .fill_width()
            .align(Align::Center)
            .gap(tokens::SPACE_3)
            .padding(Sides::x(tokens::SPACE_3))
            .fill(palette.background.with_alpha(0.80))
            .stroke(palette.border.with_alpha(0.6))
            .radius(12.0)
    }

    fn on_event(&mut self, event: UiEvent) {
        for ws in &self.workspaces {
            if event.is_click_or_activate(&format!("ws-{}", ws.slot)) {
                self.pending_activate = Some(ws.slot);
            }
        }
    }
}

// Widest Inter digit advance per text role, mirroring the role recipes
// in damascene's `apply_text_role` (label = TEXT_SM/Medium, caption =
// TEXT_XS/Regular). Measured once; used to emulate tabular numerals.
static DIGIT_W_LABEL: LazyLock<f32> = LazyLock::new(|| {
    max_digit_width(tokens::TEXT_SM.size, FontWeight::Medium)
});
static DIGIT_W_CAPTION: LazyLock<f32> = LazyLock::new(|| {
    max_digit_width(tokens::TEXT_XS.size, FontWeight::Regular)
});
/// Natural advance of '%' at caption size; the gauge column is sized
/// to exactly fit "100%" (3 digit slots + this).
static PCT_GLYPH_W: LazyLock<f32> =
    LazyLock::new(|| line_width("%", tokens::TEXT_XS.size, FontWeight::Regular, false));

fn max_digit_width(size: f32, weight: FontWeight) -> f32 {
    (0..10u8)
        .map(|d| line_width(&d.to_string(), size, weight, false))
        .fold(0.0, f32::max)
}

/// Emulated tabular numerals: digits (and pad spaces) each occupy a
/// fixed slot of the widest digit's width, centered like real `tnum`
/// figures; other glyphs (`:`/`%`) keep their natural advance. Value
/// changes can never reflow the surrounding layout.
fn tabular(s: &str, digit_w: f32, mk: &dyn Fn(String) -> El) -> El {
    let cells: Vec<El> = s
        .chars()
        .map(|c| {
            if c == ' ' {
                row(Vec::<El>::new()).width(Size::Fixed(digit_w))
            } else if c.is_ascii_digit() {
                row([mk(c.to_string())])
                    .width(Size::Fixed(digit_w))
                    .justify(Justify::Center)
                    .align(Align::Center)
            } else {
                mk(c.to_string())
            }
        })
        .collect();
    row(cells).align(Align::Center)
}

/// icon + a tight percent-over-bar column. Stacking the number on its
/// own gauge keeps them reading as one unit at any value — a low
/// percentage can't drift toward the neighboring module. Both rows
/// share one fixed width (sized to "100%"), digits right-aligned, so
/// nothing moves as values change. The fill shifts to the destructive
/// accent past the module's `hot` threshold (percent).
fn gauge_module(svg: &SvgIcon, frac: f32, hot: u32, palette: &Palette, label: Option<&str>) -> El {
    let fill = if frac * 100.0 >= hot as f32 {
        palette.destructive
    } else {
        palette.primary
    };
    let width = 3.0 * *DIGIT_W_CAPTION + *PCT_GLYPH_W;
    let gauge = column([
        tabular(&format!("{:>3.0}%", frac * 100.0), *DIGIT_W_CAPTION, &|s| {
            text(s).caption().muted()
        }),
        progress(frac, fill).height(Size::Fixed(4.0)),
    ])
    .width(Size::Fixed(width))
    .gap(3.0);

    let mut items = vec![icon(svg.clone())];
    if let Some(label) = label {
        items.push(text(label.to_string()).caption().muted());
    }
    items.push(gauge);
    row(items).gap(tokens::SPACE_1).align(Align::Center)
}

/// `abc…xyz` truncation that keeps both ends of long titles readable.
fn middle_truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1) / 2;
    let head: String = s.chars().take(keep).collect();
    let tail: String = s
        .chars()
        .skip(count - keep)
        .collect();
    format!("{head}…{tail}")
}
