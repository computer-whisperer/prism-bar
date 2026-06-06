//! The tray-menu popup's damascene `App` — one dbusmenu tree with
//! drill-down submenu navigation.
//!
//! The host owns the popup surface and sizes it once from
//! [`MenuApp::desired_size`] (the root level); deeper levels render in
//! the same surface and scroll when they don't fit. Clicks on leaves
//! come back out through [`MenuApp::take_clicked`]; submenu and back
//! rows just change which level builds next frame.

use std::rc::Rc;

use damascene_core::prelude::*;

use crate::config::Appearance;
use crate::tray::MenuNode;
use crate::ui::theme_of;

/// Fixed popup width, logical px.
pub const MENU_WIDTH: f32 = 260.0;
/// Height clamp; longer levels scroll.
pub const MENU_MAX_HEIGHT: f32 = 520.0;

/// `MetricsRole::MenuItem` default row height.
const ITEM_H: f32 = 30.0;
/// `dropdown_menu_separator`: 1px line + SPACE_1 padding either side.
const SEP_H: f32 = 1.0 + 2.0 * tokens::SPACE_1;

pub struct MenuApp {
    appearance: Rc<Appearance>,
    root: MenuNode,
    /// Node ids drilled into, root-first.
    path: Vec<i32>,
    /// Clicked leaf id, drained by the host (which closes the popup).
    clicked: Option<i32>,
}

impl MenuApp {
    pub fn new(appearance: Rc<Appearance>, root: MenuNode) -> Self {
        Self {
            appearance,
            root,
            path: Vec::new(),
            clicked: None,
        }
    }

    pub fn take_clicked(&mut self) -> Option<i32> {
        self.clicked.take()
    }

    /// The submenu level the path points at.
    fn level(&self) -> &MenuNode {
        let mut node = &self.root;
        for id in &self.path {
            match node.children.iter().find(|c| c.id == *id) {
                Some(child) => node = child,
                None => return node, // tree changed under us; stay put
            }
        }
        node
    }

    /// Logical popup size for the root level (stable for the popup's
    /// lifetime; drilled levels reuse it and scroll if longer).
    pub fn desired_size(&self) -> (f32, f32) {
        let h: f32 = self
            .root
            .children
            .iter()
            .map(|n| if n.separator { SEP_H } else { ITEM_H })
            .sum();
        // +2 for the content stroke.
        (MENU_WIDTH, (h + 2.0).clamp(ITEM_H, MENU_MAX_HEIGHT))
    }
}

impl App for MenuApp {
    fn theme(&self) -> Theme {
        theme_of(self.appearance.theme)
    }

    fn build(&self, _cx: &BuildCx) -> El {
        let level = self.level();
        let mut rows: Vec<El> = Vec::with_capacity(level.children.len() + 2);
        if !self.path.is_empty() {
            rows.push(
                dropdown_menu_item([text("‹").label().muted(), dropdown_menu_item_label("Back")])
                    .key("menu-back"),
            );
            rows.push(dropdown_menu_separator());
        }
        for node in &level.children {
            rows.push(menu_row(node));
        }

        // The wl_surface is sized by the host to `desired_size`; fill it.
        dropdown_menu_content([scroll(rows).width(Size::Fill(1.0)).height(Size::Fill(1.0))])
            .radius(self.appearance.radius.min(10.0))
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    fn on_event(&mut self, event: UiEvent, _cx: &EventCx) {
        if event.is_click_or_activate("menu-back") {
            self.path.pop();
            return;
        }
        // Identify the clicked row among the current level's nodes.
        let clicked = self
            .level()
            .children
            .iter()
            .find(|n| event.is_click_or_activate(&format!("menu-{}", n.id)));
        if let Some(node) = clicked {
            if node.children.is_empty() {
                self.clicked = Some(node.id);
            } else {
                self.path.push(node.id);
            }
        }
    }
}

fn menu_row(node: &MenuNode) -> El {
    if node.separator {
        return dropdown_menu_separator();
    }
    let mut cells: Vec<El> = Vec::with_capacity(3);
    if node.toggle == Some(true) {
        cells.push(text("✓").label().width(Size::Hug));
    }
    let label = dropdown_menu_item_label(node.label.clone());
    cells.push(if node.enabled { label } else { label.muted() });
    if !node.children.is_empty() {
        cells.push(text("›").label().muted().width(Size::Hug));
    }
    let row = dropdown_menu_item(cells);
    if node.enabled {
        row.key(format!("menu-{}", node.id))
    } else {
        row
    }
}
