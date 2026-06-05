//! The bar's damascene `App` — pure state → tree projection.
//!
//! Spike scope: a fake workspace strip (click to switch, proving
//! pointer routing + hover/press animation), a title, and a live
//! clock (proving the timed-redraw path). Real prism IPC state
//! replaces the fakes once the host is validated.

use damascene_core::prelude::*;

pub struct BarApp {
    clock: String,
    active_ws: u8,
}

impl BarApp {
    pub fn new() -> Self {
        Self {
            clock: String::new(),
            active_ws: 1,
        }
    }
}

impl App for BarApp {
    fn before_build(&mut self) {
        self.clock = chrono::Local::now().format("%H:%M:%S").to_string();
    }

    fn build(&self, cx: &BuildCx) -> El {
        let palette = cx.palette();
        let pills: Vec<El> = (1..=4)
            .map(|i| {
                let b = button(i.to_string()).key(format!("ws-{i}"));
                if i == self.active_ws {
                    b.primary()
                } else {
                    b.ghost()
                }
            })
            .collect();

        // The wl_surface is cleared transparent; the visible bar is this
        // rounded translucent panel, floated off the screen edge by the
        // layer-surface margins set in the host.
        row([
            text("prism").label().muted(),
            row(pills).gap(tokens::SPACE_1),
            spacer(),
            text(self.clock.clone()).label(),
        ])
        .fill_width()
        .align(Align::Center)
        .gap(tokens::SPACE_3)
        .padding(Sides::x(tokens::SPACE_3))
        .fill(palette.background.with_alpha(0.80))
        .stroke(palette.border.with_alpha(0.6))
        .radius(12.0)
    }

    fn on_event(&mut self, event: UiEvent) {
        for i in 1..=4u8 {
            if event.is_click_or_activate(&format!("ws-{i}")) {
                self.active_ws = i;
            }
        }
    }
}
