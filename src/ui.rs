//! The bar's damascene `App` — pure state → tree projection.
//!
//! The host pushes compositor state in via [`BarApp::set_state`] before
//! each build; clicks come back out through [`BarApp::take_activate`].
//! Modules degrade by absence: no workspaces → no pills, no focused
//! window → no title.

use std::sync::LazyLock;

use damascene_core::prelude::*;
use damascene_core::SvgIcon;

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
    clock: String,
    workspaces: Vec<WorkspaceView>,
    title: Option<String>,
    sys: SysStats,
    /// Workspace slot the user clicked, drained by the host.
    pending_activate: Option<usize>,
}

impl BarApp {
    pub fn new() -> Self {
        Self {
            clock: String::new(),
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
        self.clock = chrono::Local::now().format("%H:%M:%S").to_string();
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
        for (svg, frac) in [
            (&ICON_CPU, self.sys.cpu),
            (&ICON_MEM, self.sys.mem),
            (&ICON_DISK, self.sys.disk),
        ] {
            if let Some(frac) = frac {
                items.push(gauge_module(svg, frac, palette));
            }
        }
        // Monospace digits: every glyph has the same advance, so the
        // ticking clock never reflows the row.
        items.push(mono(self.clock.clone()).label());

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

/// icon + mini gauge + percentage. The gauge fill shifts to the
/// destructive accent as the resource runs hot.
fn gauge_module(svg: &SvgIcon, frac: f32, palette: &Palette) -> El {
    let fill = if frac >= 0.90 {
        palette.destructive
    } else {
        palette.primary
    };
    row([
        icon(svg.clone()),
        progress(frac, fill)
            .width(Size::Fixed(42.0))
            .height(Size::Fixed(5.0)),
        // Monospace + space-padded to 3 digits: "  4%" and "100%" are
        // the same width, so value changes never shift the layout.
        mono(format!("{:>3.0}%", frac * 100.0)).caption().muted(),
    ])
    .gap(tokens::SPACE_1)
    .align(Align::Center)
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
