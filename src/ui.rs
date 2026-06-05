//! The bar's damascene `App` — pure state → tree projection.
//!
//! The host pushes compositor state in via [`BarApp::set_state`] before
//! each build; clicks come back out through [`BarApp::take_activate`].
//! Modules degrade by absence: no workspaces → no pills, no focused
//! window → no title.

use damascene_core::prelude::*;

use crate::workspaces::WorkspaceView;

/// Longest title before middle-truncation; keeps the title from
/// crowding the clock until proper width-constrained truncation.
const TITLE_MAX_CHARS: usize = 80;

pub struct BarApp {
    clock: String,
    workspaces: Vec<WorkspaceView>,
    title: Option<String>,
    /// Workspace slot the user clicked, drained by the host.
    pending_activate: Option<usize>,
}

impl BarApp {
    pub fn new() -> Self {
        Self {
            clock: String::new(),
            workspaces: Vec::new(),
            title: None,
            pending_activate: None,
        }
    }

    /// Host-side state push, called before each build.
    pub fn set_state(&mut self, workspaces: Vec<WorkspaceView>, title: Option<String>) {
        self.workspaces = workspaces;
        self.title = title;
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
        items.push(text(self.clock.clone()).label());

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
