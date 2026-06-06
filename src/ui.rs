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

use crate::config::{Appearance, Module, ThemeName};
use crate::sysmon::SysStats;
use crate::tray::{Address, TrayCmd, TrayIcon, TrayItem};
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

/// A tray interaction the host must perform (it owns the D-Bus side).
pub enum TrayAction {
    /// Forward to the tray thread as-is.
    Cmd(TrayCmd),
    /// Open the item's menu: fetch the layout and pop a menu surface
    /// anchored at the icon's laid-out rect (bar surface coordinates).
    OpenMenu { address: Address, anchor: Rect },
}

pub struct BarApp {
    /// Right-cluster modules in display order (from config).
    modules: Rc<Vec<Module>>,
    appearance: Rc<Appearance>,
    /// Formatted text per Clock module, in module order.
    clocks: Vec<String>,
    workspaces: Vec<WorkspaceView>,
    title: Option<String>,
    sys: SysStats,
    tray: Vec<TrayItem>,
    /// Workspace slot the user clicked, drained by the host.
    pending_activate: Option<usize>,
    /// Tray interaction from the last event batch, drained by the host.
    pending_tray: Option<TrayAction>,
}

impl BarApp {
    pub fn new(modules: Rc<Vec<Module>>, appearance: Rc<Appearance>) -> Self {
        Self {
            modules,
            appearance,
            clocks: Vec::new(),
            workspaces: Vec::new(),
            title: None,
            sys: SysStats::default(),
            tray: Vec::new(),
            pending_activate: None,
            pending_tray: None,
        }
    }

    /// Host-side state push, called before each build.
    pub fn set_state(
        &mut self,
        workspaces: Vec<WorkspaceView>,
        title: Option<String>,
        sys: SysStats,
        tray: Vec<TrayItem>,
    ) {
        self.workspaces = workspaces;
        self.title = title;
        self.sys = sys;
        self.tray = tray;
    }

    /// Drain the workspace-switch request from the last event batch.
    pub fn take_activate(&mut self) -> Option<usize> {
        self.pending_activate.take()
    }

    /// Drain the tray interaction from the last event batch.
    pub fn take_tray_action(&mut self) -> Option<TrayAction> {
        self.pending_tray.take()
    }
}

impl App for BarApp {
    fn theme(&self) -> Theme {
        match self.appearance.theme {
            ThemeName::Dark => Theme::damascene_dark(),
            ThemeName::Light => Theme::damascene_light(),
            ThemeName::SlateBlueDark => Theme::radix_slate_blue_dark(),
            ThemeName::SlateBlueLight => Theme::radix_slate_blue_light(),
            ThemeName::SandAmberDark => Theme::radix_sand_amber_dark(),
            ThemeName::SandAmberLight => Theme::radix_sand_amber_light(),
            ThemeName::MauveVioletDark => Theme::radix_mauve_violet_dark(),
            ThemeName::MauveVioletLight => Theme::radix_mauve_violet_light(),
        }
    }

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
            let t = middle_truncate(t, self.appearance.title_max_length);
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
        // Right cluster gets its own row with a wider gap so the
        // module groups separate clearly.
        let mut right: Vec<El> = Vec::new();
        let mut clock_i = 0;
        for module in self.modules.iter() {
            match module {
                Module::Tray(o) => {
                    if !self.tray.is_empty() {
                        let icons: Vec<El> = self
                            .tray
                            .iter()
                            .enumerate()
                            .map(|(i, item)| tray_icon(i, item, o.icon_size as f32))
                            .collect();
                        right.push(row(icons).gap(tokens::SPACE_2).align(Align::Center));
                    }
                }
                Module::Cpu(o) => {
                    if let Some(frac) = self.sys.cpu {
                        right.push(gauge_module(
                            &ICON_CPU,
                            frac,
                            o.hot,
                            o.width,
                            o.thickness,
                            palette,
                            None,
                        ));
                    }
                }
                Module::Memory(o) => {
                    if let Some(frac) = self.sys.mem {
                        right.push(gauge_module(
                            &ICON_MEM,
                            frac,
                            o.hot,
                            o.width,
                            o.thickness,
                            palette,
                            None,
                        ));
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
                        right.push(gauge_module(
                            &ICON_DISK,
                            frac,
                            o.hot,
                            o.width,
                            o.thickness,
                            palette,
                            label,
                        ));
                    }
                }
                Module::Clock(_) => {
                    if let Some(clock) = self.clocks.get(clock_i) {
                        right.push(tabular(clock, *DIGIT_W_LABEL, &|s| text(s).label()));
                    }
                    clock_i += 1;
                }
            }
        }
        items.push(row(right).gap(tokens::SPACE_5).align(Align::Center));

        // The wl_surface is cleared transparent; the visible bar is this
        // rounded translucent panel, floated off the screen edge by the
        // layer-surface margins set in the host.
        let mut panel = row(items)
            .fill_width()
            .align(Align::Center)
            .gap(tokens::SPACE_3)
            .padding(Sides::x(tokens::SPACE_3))
            .fill(palette.background.with_alpha(self.appearance.opacity))
            .radius(self.appearance.radius);
        if self.appearance.border {
            panel = panel.stroke(palette.border.with_alpha(0.6));
        }
        panel
    }

    fn on_event(&mut self, event: UiEvent, _cx: &EventCx) {
        for ws in &self.workspaces {
            if event.is_click_or_activate(&format!("ws-{}", ws.slot)) {
                self.pending_activate = Some(ws.slot);
            }
        }
        for (i, item) in self.tray.iter().enumerate() {
            if !event.is_route(&format!("tray-{i}")) {
                continue;
            }
            let open_menu = || {
                event.target_rect().map(|anchor| TrayAction::OpenMenu {
                    address: item.address.clone(),
                    anchor,
                })
            };
            // SNI conventions: left = Activate (menu when the item asks
            // for it), right = context menu, middle = SecondaryActivate.
            self.pending_tray = match event.kind {
                UiEventKind::Click | UiEventKind::Activate => {
                    if item.item_is_menu && item.has_menu {
                        open_menu()
                    } else {
                        Some(TrayAction::Cmd(TrayCmd::Activate(item.address.clone())))
                    }
                }
                UiEventKind::SecondaryClick if item.has_menu => open_menu(),
                UiEventKind::MiddleClick => Some(TrayAction::Cmd(TrayCmd::SecondaryActivate(
                    item.address.clone(),
                ))),
                _ => None,
            }
            .or(self.pending_tray.take());
        }
    }
}

/// One tray icon as a keyed, clickable element. Items without a
/// resolvable icon fall back to the title's first letter so they stay
/// visible and clickable.
fn tray_icon(i: usize, item: &TrayItem, size: f32) -> El {
    let el = match &item.icon {
        Some(TrayIcon::Raster(img)) => image(img.clone())
            .image_fit(ImageFit::Contain)
            .width(Size::Fixed(size))
            .height(Size::Fixed(size)),
        Some(TrayIcon::Svg(svg)) => icon(svg.clone()).icon_size(size),
        None => row([text(
            item.title
                .chars()
                .next()
                .unwrap_or('?')
                .to_uppercase()
                .to_string(),
        )
        .label()
        .muted()])
        .width(Size::Fixed(size))
        .height(Size::Fixed(size))
        .justify(Justify::Center)
        .align(Align::Center),
    };
    let el = el.key(format!("tray-{i}"));
    if item.title.is_empty() {
        el
    } else {
        el.tooltip(item.title.clone())
    }
}

// Widest Inter digit advance per text role, mirroring the role recipes
// in damascene's `apply_text_role` (label = TEXT_SM/Medium, caption =
// TEXT_XS/Regular). Measured once; used to emulate tabular numerals.
static DIGIT_W_LABEL: LazyLock<f32> =
    LazyLock::new(|| max_digit_width(tokens::TEXT_SM.size, FontWeight::Medium));
static DIGIT_W_CAPTION: LazyLock<f32> =
    LazyLock::new(|| max_digit_width(tokens::TEXT_XS.size, FontWeight::Regular));

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

/// icon + percent + bar. The percent sits between icon and bar with
/// digits right-aligned in fixed slots, so the visible digits stay
/// pinned against their own bar at any value — the unused pad slots
/// fall next to the icon, inside the module, where they read as
/// number alignment. Nothing moves as values change, and both static
/// anchors (icon, bar) bracket the variable part. The fill shifts to
/// the destructive accent past the module's `hot` threshold (percent).
fn gauge_module(
    svg: &SvgIcon,
    frac: f32,
    hot: u32,
    width: u32,
    thickness: u32,
    palette: &Palette,
    label: Option<&str>,
) -> El {
    let fill = if frac * 100.0 >= hot as f32 {
        palette.destructive
    } else {
        palette.primary
    };
    let mut items = vec![icon(svg.clone())];
    if let Some(label) = label {
        items.push(text(label.to_string()).caption().muted());
    }
    // Two digit slots: 0-99 cover steady state with minimal slack next
    // to the icon. A pegged 100% widens the module by one slot — rare
    // enough that the disturbance is earned.
    items.push(tabular(
        &format!("{:>2.0}%", frac * 100.0),
        *DIGIT_W_CAPTION,
        &|s| text(s).caption().muted(),
    ));
    items.push(
        progress(frac, fill)
            .width(Size::Fixed(width as f32))
            .height(Size::Fixed(thickness as f32)),
    );
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
    let tail: String = s.chars().skip(count - keep).collect();
    format!("{head}…{tail}")
}
