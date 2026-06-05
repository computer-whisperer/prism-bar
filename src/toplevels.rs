//! wlr-foreign-toplevel-management client state — focused window title.
//!
//! Per the protocol, property events (title, app_id, state) buffer up
//! and the per-handle `done` event applies them atomically; we mark the
//! bar dirty only on `done`/`closed` so renders see consistent state.
//!
//! Missing global → no manager; the title module doesn't render.

use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalList;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    self, ZwlrForeignToplevelHandleV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    self, ZwlrForeignToplevelManagerV1,
};

use crate::Bar;

/// `zwlr_foreign_toplevel_handle_v1.state` enum value for "activated".
const STATE_ACTIVATED: u32 = 2;

#[derive(Default)]
pub struct ToplevelsState {
    manager: Option<ZwlrForeignToplevelManagerV1>,
    toplevels: Vec<ToplevelData>,
}

struct ToplevelData {
    handle: ZwlrForeignToplevelHandleV1,
    // Applied state (post-`done`).
    title: Option<String>,
    app_id: Option<String>,
    activated: bool,
    // Pending changes since the last `done` (None = not re-sent, keep
    // the applied value).
    pending_title: Option<String>,
    pending_app_id: Option<String>,
    pending_activated: Option<bool>,
}

impl ToplevelsState {
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<Bar>) -> Self {
        let manager = match globals.bind::<ZwlrForeignToplevelManagerV1, Bar, ()>(qh, 1..=3, ()) {
            Ok(m) => Some(m),
            Err(err) => {
                tracing::info!(
                    "wlr-foreign-toplevel-management unavailable ({err}); title module disabled"
                );
                None
            }
        };
        Self {
            manager,
            ..Default::default()
        }
    }

    /// Title (preferred) or app id of the activated toplevel.
    pub fn focused_title(&self) -> Option<String> {
        let focused = self.toplevels.iter().find(|t| t.activated)?;
        focused.title.clone().or_else(|| focused.app_id.clone())
    }

    fn toplevel_mut(&mut self, id: &ObjectId) -> Option<&mut ToplevelData> {
        self.toplevels.iter_mut().find(|t| t.handle.id() == *id)
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Bar {
    fn event(
        bar: &mut Self,
        _manager: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                bar.toplevels.toplevels.push(ToplevelData {
                    handle: toplevel,
                    title: None,
                    app_id: None,
                    activated: false,
                    pending_title: None,
                    pending_app_id: None,
                    pending_activated: None,
                });
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                bar.toplevels.manager = None;
                bar.toplevels.toplevels.clear();
                bar.dirty = true;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(Bar, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for Bar {
    fn event(
        bar: &mut Self,
        toplevel: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = toplevel.id();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(t) = bar.toplevels.toplevel_mut(&id) {
                    t.pending_title = Some(title);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                if let Some(t) = bar.toplevels.toplevel_mut(&id) {
                    t.pending_app_id = Some(app_id);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state } => {
                if let Some(t) = bar.toplevels.toplevel_mut(&id) {
                    // wl_array of native-endian u32 enum values.
                    t.pending_activated = Some(
                        state
                            .chunks_exact(4)
                            .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
                            .any(|s| s == STATE_ACTIVATED),
                    );
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                if let Some(t) = bar.toplevels.toplevel_mut(&id) {
                    if let Some(title) = t.pending_title.take() {
                        t.title = Some(title);
                    }
                    if let Some(app_id) = t.pending_app_id.take() {
                        t.app_id = Some(app_id);
                    }
                    if let Some(activated) = t.pending_activated.take() {
                        t.activated = activated;
                    }
                    bar.dirty = true;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                bar.toplevels.toplevels.retain(|t| t.handle.id() != id);
                toplevel.destroy();
                bar.dirty = true;
            }
            _ => {}
        }
    }
}
