//! ext-workspace-v1 client state.
//!
//! Tracks workspace groups and workspaces from the compositor. The
//! protocol is double-buffered: property events mutate pending state,
//! and the manager's `done` event applies the batch — we bump `dirty`
//! only there, so the bar never renders a half-applied transaction.
//!
//! Missing global → `WorkspacesState::default()` with no manager; the
//! workspace module simply doesn't render.

use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::{
    self, ExtWorkspaceGroupHandleV1,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::{
    self, ExtWorkspaceHandleV1, State,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::{
    self, ExtWorkspaceManagerV1,
};
use wayland_client::backend::ObjectId;

use crate::Bar;

#[derive(Default)]
pub struct WorkspacesState {
    manager: Option<ExtWorkspaceManagerV1>,
    groups: Vec<Group>,
    workspaces: Vec<WorkspaceData>,
}

struct Group {
    handle: ExtWorkspaceGroupHandleV1,
    outputs: Vec<WlOutput>,
    workspaces: Vec<ObjectId>,
}

struct WorkspaceData {
    handle: ExtWorkspaceHandleV1,
    name: Option<String>,
    coordinates: Vec<u32>,
    state: State,
}

/// One workspace as the UI sees it, post-`done`, sorted for display.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceView {
    pub label: String,
    pub active: bool,
    pub urgent: bool,
    pub hidden: bool,
    /// Index into `WorkspacesState::workspaces` for activation requests.
    pub slot: usize,
}

impl WorkspacesState {
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<Bar>) -> Self {
        let manager = match globals.bind::<ExtWorkspaceManagerV1, Bar, ()>(qh, 1..=1, ()) {
            Ok(m) => Some(m),
            Err(err) => {
                tracing::info!("ext-workspace-v1 unavailable ({err}); workspace module disabled");
                None
            }
        };
        Self {
            manager,
            ..Default::default()
        }
    }

    /// Build the display list: the bar's output's group when known,
    /// any-group fallback otherwise. Sorted by protocol coordinates,
    /// labeled by name or 1-based position.
    pub fn snapshot(&self, bar_output: Option<&WlOutput>) -> Vec<WorkspaceView> {
        // Workspaces in a group on our output; if we can't match a
        // group (no output info yet, or no groups), show everything.
        let on_output: Option<&Group> = bar_output.and_then(|out| {
            self.groups
                .iter()
                .find(|g| g.outputs.iter().any(|o| o == out))
        });

        let mut slots: Vec<usize> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, ws)| match on_output {
                Some(g) => g.workspaces.contains(&ws.handle.id()),
                None => true,
            })
            .map(|(i, _)| i)
            .collect();
        slots.sort_by(|&a, &b| {
            let (wa, wb) = (&self.workspaces[a], &self.workspaces[b]);
            wa.coordinates
                .cmp(&wb.coordinates)
                .then_with(|| wa.name.cmp(&wb.name))
                .then(a.cmp(&b))
        });

        slots
            .into_iter()
            .enumerate()
            .map(|(pos, slot)| {
                let ws = &self.workspaces[slot];
                WorkspaceView {
                    label: ws.name.clone().unwrap_or_else(|| (pos + 1).to_string()),
                    active: ws.state.contains(State::Active),
                    urgent: ws.state.contains(State::Urgent),
                    hidden: ws.state.contains(State::Hidden),
                    slot,
                }
            })
            .collect()
    }

    /// Ask the compositor to switch to the workspace at `slot`.
    pub fn activate(&self, slot: usize) {
        let (Some(manager), Some(ws)) = (&self.manager, self.workspaces.get(slot)) else {
            return;
        };
        ws.handle.activate();
        manager.commit();
    }

    fn workspace_mut(&mut self, id: &ObjectId) -> Option<&mut WorkspaceData> {
        self.workspaces.iter_mut().find(|w| w.handle.id() == *id)
    }

    fn group_mut(&mut self, id: &ObjectId) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.handle.id() == *id)
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for Bar {
    fn event(
        bar: &mut Self,
        _manager: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                bar.workspaces.groups.push(Group {
                    handle: workspace_group,
                    outputs: Vec::new(),
                    workspaces: Vec::new(),
                });
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                bar.workspaces.workspaces.push(WorkspaceData {
                    handle: workspace,
                    name: None,
                    coordinates: Vec::new(),
                    state: State::empty(),
                });
            }
            ext_workspace_manager_v1::Event::Done => {
                // Transaction boundary — everything since the previous
                // `done` becomes visible at once.
                bar.dirty = true;
            }
            ext_workspace_manager_v1::Event::Finished => {
                bar.workspaces.manager = None;
                bar.workspaces.groups.clear();
                bar.workspaces.workspaces.clear();
                bar.dirty = true;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(Bar, ExtWorkspaceManagerV1, [
        ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => (ExtWorkspaceGroupHandleV1, ()),
        ext_workspace_manager_v1::EVT_WORKSPACE_OPCODE => (ExtWorkspaceHandleV1, ()),
    ]);
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for Bar {
    fn event(
        bar: &mut Self,
        group: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = group.id();
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                if let Some(g) = bar.workspaces.group_mut(&id) {
                    g.outputs.push(output);
                }
            }
            ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
                if let Some(g) = bar.workspaces.group_mut(&id) {
                    g.outputs.retain(|o| o != &output);
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                if let Some(g) = bar.workspaces.group_mut(&id) {
                    g.workspaces.push(workspace.id());
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                if let Some(g) = bar.workspaces.group_mut(&id) {
                    g.workspaces.retain(|w| w != &workspace.id());
                }
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                bar.workspaces.groups.retain(|g| g.handle.id() != id);
                group.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for Bar {
    fn event(
        bar: &mut Self,
        workspace: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = workspace.id();
        match event {
            ext_workspace_handle_v1::Event::Name { name } => {
                if let Some(ws) = bar.workspaces.workspace_mut(&id) {
                    ws.name = Some(name);
                }
            }
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                if let Some(ws) = bar.workspaces.workspace_mut(&id) {
                    // wl_array of native-endian u32s, most-significant
                    // axis first per the protocol.
                    ws.coordinates = coordinates
                        .chunks_exact(4)
                        .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
                        .collect();
                }
            }
            ext_workspace_handle_v1::Event::State { state } => {
                if let (Some(ws), WEnum::Value(state)) =
                    (bar.workspaces.workspace_mut(&id), state)
                {
                    ws.state = state;
                }
            }
            ext_workspace_handle_v1::Event::Removed => {
                bar.workspaces.workspaces.retain(|w| w.handle.id() != id);
                for g in &mut bar.workspaces.groups {
                    g.workspaces.retain(|w| w != &id);
                }
                workspace.destroy();
            }
            _ => {}
        }
    }
}
