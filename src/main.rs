//! prism-bar host: wlr-layer-shell surfaces driven by the damascene
//! wgpu `Runner` (the custom-host path — `damascene-winit-wgpu` only
//! creates regular toplevels, so we own the surfaces and event loop).
//!
//! Shape:
//!
//!   one Bar (wayland conn, protocol state, shared wgpu device, config)
//!     → one BarSurface per configured output (SCTK output hotplug)
//!       → layer surface + wgpu swapchain + damascene Runner + BarApp
//!   per frame: app.build() → runner.prepare() → runner.render()
//!   calloop: wayland socket + redraw deadlines (animation, clock)
//!   SCTK pointer events → routed by wl_surface → runner.pointer_*()

mod config;
mod menu;
mod sysmon;
mod toplevels;
mod tray;
mod ui;
mod workspaces;

use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel as calloop_channel;
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_xdg_popup, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::xdg_positioner::{
    Anchor as XdgAnchor, ConstraintAdjustment, Gravity,
};

use damascene_core::event::{Pointer, PointerButton};
use damascene_core::prelude::{App, Rect, Theme};
use damascene_core::BuildCx;
use damascene_wgpu::{MsaaTarget, Runner, RunnerCaps};

use crate::config::{Appearance, Config, Module, Position};
use crate::menu::MenuApp;
use crate::sysmon::SysMon;
use crate::toplevels::ToplevelsState;
use crate::tray::{Address, MenuNode, Tray, TrayCmd, TrayEvent, TrayItem};
use crate::ui::{BarApp, TrayAction};
use crate::workspaces::WorkspacesState;

const MSAA_SAMPLES: u32 = 4;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "prism_bar=info".into()),
        )
        .init();

    let config = Config::load()?;
    let modules = Rc::new(config.modules());
    let appearance = Rc::new(config.appearance());
    let sample_interval = config.sample_interval;

    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, event_queue) = registry_queue_init::<Bar>(&conn).context("registry init")?;
    let qh = event_queue.handle();

    // Tray events flow over this channel for the life of the process;
    // the tray thread itself starts/stops with the config (ensure_tray).
    let (tray_send, tray_recv) = calloop_channel::channel();

    let mut bar = Bar {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).context("wl_compositor")?,
        layer_shell: LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1")?,
        xdg_shell: XdgShell::bind(&globals, &qh).context("xdg_wm_base")?,
        workspaces: WorkspacesState::bind(&globals, &qh),
        toplevels: ToplevelsState::bind(&globals, &qh),
        conn: conn.clone(),
        config,
        instance: wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()),
        gpu: None,
        surfaces: Vec::new(),
        pointer: None,
        sysmon: SysMon::new(&modules, sample_interval),
        has_clock: modules.iter().any(|m| matches!(m, Module::Clock(_))),
        modules,
        appearance,
        tray: None,
        tray_send,
        tray_items: Vec::new(),
        pending_menu: None,
        menu: None,
        seat: None,
        last_press_serial: 0,
        reload_at: None,
        dirty: false,
        last_clock_secs: 0,
        exit: false,
    };
    bar.ensure_tray();

    let mut event_loop: EventLoop<Bar> = EventLoop::try_new().context("calloop")?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {e}"))?;
    watch_config(&mut event_loop)?;
    let tray_qh = qh.clone();
    event_loop
        .handle()
        .insert_source(tray_recv, move |event, _, bar: &mut Bar| {
            if let calloop_channel::Event::Msg(event) = event {
                bar.on_tray_event(&tray_qh, event);
            }
        })
        .map_err(|e| anyhow::anyhow!("insert tray source: {e}"))?;

    // Surfaces are created by `new_output` as outputs are announced
    // (the same path handles initial enumeration and later hotplug).
    while !bar.exit {
        // Sleep until the earliest deadline: damascene animation on any
        // surface, or the clock's next second. Wayland events interrupt.
        let now = Instant::now();
        let mut timeout = Duration::from_secs(3600);
        if bar.has_clock {
            timeout = timeout.min(Duration::from_millis(
                1000 - chrono::Local::now().timestamp_subsec_millis() as u64,
            ));
        }
        if bar.sysmon.active() {
            timeout = timeout.min(bar.sysmon.next_sample.saturating_duration_since(now));
        }
        if let Some(d) = bar.reload_at {
            timeout = timeout.min(d.saturating_duration_since(now));
        }
        for s in &bar.surfaces {
            if let Some(d) = s.anim_deadline {
                timeout = timeout.min(d.saturating_duration_since(now));
            }
        }
        if let Some(d) = bar.menu.as_ref().and_then(|m| m.anim_deadline) {
            timeout = timeout.min(d.saturating_duration_since(now));
        }

        event_loop
            .dispatch(Some(timeout), &mut bar)
            .context("event loop dispatch")?;

        // Global state change (workspaces, toplevels) → every bar.
        if bar.dirty {
            bar.dirty = false;
            for s in &mut bar.surfaces {
                s.dirty = true;
            }
        }
        // Debounced config reload (armed by the inotify source).
        if bar.reload_at.is_some_and(|d| d <= Instant::now()) {
            bar.reload_at = None;
            bar.reload_config(&qh);
        }
        // Clock tick: redraw when the displayed second changes.
        let secs = chrono::Local::now().timestamp();
        if bar.has_clock && secs != bar.last_clock_secs {
            bar.last_clock_secs = secs;
            for s in &mut bar.surfaces {
                s.dirty = true;
            }
        }
        // System monitor resample.
        if bar.sysmon.active() && Instant::now() >= bar.sysmon.next_sample && bar.sysmon.sample() {
            for s in &mut bar.surfaces {
                s.dirty = true;
            }
        }
        // An animation deadline elapsing is a per-surface redraw reason.
        let now = Instant::now();
        for s in &mut bar.surfaces {
            if s.anim_deadline.is_some_and(|d| d <= now) {
                s.anim_deadline = None;
                s.dirty = true;
            }
        }
        if let Some(m) = bar.menu.as_mut() {
            if m.anim_deadline.is_some_and(|d| d <= now) {
                m.anim_deadline = None;
                m.dirty = true;
            }
        }

        for i in 0..bar.surfaces.len() {
            if bar.surfaces[i].dirty {
                bar.draw(i);
            }
        }
        if bar.menu.as_ref().is_some_and(|m| m.dirty) {
            bar.draw_menu();
        }
    }
    Ok(())
}

/// GPU objects shared by every bar surface (one device serves all
/// swapchains). Created lazily with the first surface, since adapter
/// selection wants a compatible surface.
struct GpuShared {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

/// Swapchain + renderer for one surface (bar or menu popup); created
/// on the surface's first configure (before that we don't know the
/// size).
struct Swapchain {
    config: wgpu::SurfaceConfiguration,
    msaa: Option<MsaaTarget>,
    runner: Runner,
}

/// Create or resize a swapchain for `(w, h)` physical pixels.
fn setup_swapchain(
    gpu: &GpuShared,
    wgpu_surface: &wgpu::Surface<'_>,
    swapchain: &mut Option<Swapchain>,
    (w, h): (u32, u32),
    theme: Theme,
    label: &str,
) {
    match swapchain {
        Some(sc) => {
            if sc.config.width == w && sc.config.height == h {
                return;
            }
            sc.config.width = w;
            sc.config.height = h;
            wgpu_surface.configure(&gpu.device, &sc.config);
            sc.runner.set_surface_size(w, h);
            let extent = wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            };
            if let Some(msaa) = sc.msaa.as_mut() {
                if !msaa.matches(extent) {
                    *msaa =
                        MsaaTarget::new(&gpu.device, sc.config.format, extent, msaa.sample_count);
                }
            }
        }
        None => {
            let caps = wgpu_surface.get_capabilities(&gpu.adapter);
            let format = caps
                .formats
                .iter()
                .copied()
                .find(|f| f.is_srgb())
                .unwrap_or(caps.formats[0]);
            // Transparent background: damascene's blend states leave
            // correct premultiplied coverage over a transparent
            // clear, so PreMultiplied is the right composite mode.
            let alpha_mode = if caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
            {
                wgpu::CompositeAlphaMode::PreMultiplied
            } else {
                tracing::warn!(
                    surface = %label,
                    modes = ?caps.alpha_modes,
                    "no premultiplied alpha; surface will be opaque"
                );
                caps.alpha_modes[0]
            };
            let config = wgpu::SurfaceConfiguration {
                // COPY_SRC matches the runner's backdrop-snapshot path.
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                format,
                width: w,
                height: h,
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode,
                view_formats: vec![],
                desired_maximum_frame_latency: 1,
            };
            wgpu_surface.configure(&gpu.device, &config);

            let mut runner = Runner::with_caps(
                &gpu.device,
                &gpu.queue,
                format,
                MSAA_SAMPLES,
                RunnerCaps::from_adapter(&gpu.adapter),
            );
            runner.set_theme(theme);
            runner.set_surface_size(w, h);
            runner.warm_default_glyphs();

            let msaa = (MSAA_SAMPLES > 1).then(|| {
                MsaaTarget::new(
                    &gpu.device,
                    format,
                    wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    MSAA_SAMPLES,
                )
            });
            tracing::info!(surface = %label, ?format, "swapchain configured");
            *swapchain = Some(Swapchain {
                config,
                msaa,
                runner,
            });
        }
    }
}

struct FrameOutcome {
    /// The frame was lost/outdated; the surface was reconfigured and
    /// the caller should redraw next loop turn.
    retry: bool,
    anim_deadline: Option<Instant>,
}

/// Build the app's tree and render one frame: build → prepare →
/// acquire → render → present.
fn render_frame<A: App>(
    gpu: &GpuShared,
    wgpu_surface: &wgpu::Surface<'_>,
    sc: &mut Swapchain,
    app: &mut A,
    (width, height): (u32, u32),
    scale: i32,
    label: &str,
) -> FrameOutcome {
    let viewport = Rect::new(0.0, 0.0, width as f32, height as f32);

    app.before_build();
    let theme = app.theme();
    let mut tree = {
        let cx = BuildCx::new(&theme)
            .with_ui_state(sc.runner.ui_state())
            .with_viewport(viewport.w, viewport.h);
        app.build(&cx)
    };
    sc.runner.set_theme(theme);
    sc.runner.set_hotkeys(app.hotkeys());

    let prepare = sc
        .runner
        .prepare(&gpu.device, &gpu.queue, &mut tree, viewport, scale as f32);

    let frame = match wgpu_surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
        wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
            wgpu_surface.configure(&gpu.device, &sc.config);
            return FrameOutcome {
                retry: true,
                anim_deadline: None,
            };
        }
        other => {
            tracing::error!(surface = %label, "surface unavailable: {other:?}");
            return FrameOutcome {
                retry: false,
                anim_deadline: None,
            };
        }
    };
    let view = frame
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("prism-bar::encoder"),
        });
    sc.runner.render(
        &gpu.device,
        &mut encoder,
        &frame.texture,
        &view,
        sc.msaa.as_ref().map(|m| &m.view),
        // Transparent clear — the visible panel is a rounded rect in
        // the tree; the compositor sees through the rest.
        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
    );
    gpu.queue.submit(Some(encoder.finish()));
    frame.present();

    let mut anim_deadline = prepare.next_redraw_in.map(|d| Instant::now() + d);
    if prepare.needs_redraw && anim_deadline.is_none() {
        anim_deadline = Some(Instant::now());
    }
    FrameOutcome {
        retry: false,
        anim_deadline,
    }
}

struct BarSurface {
    // Drop order: the wgpu surface (unsafely) borrows the wl_surface
    // kept alive by `layer`, so it must drop first.
    wgpu_surface: wgpu::Surface<'static>,
    swapchain: Option<Swapchain>,
    layer: LayerSurface,
    output: wl_output::WlOutput,
    output_name: String,
    app: BarApp,
    /// Logical (surface-coordinate) size.
    width: u32,
    height: u32,
    /// Integer buffer scale from the compositor.
    scale: i32,
    dirty: bool,
    /// When damascene wants the next animation frame.
    anim_deadline: Option<Instant>,
    /// Last pointer position in logical px (button events don't carry one).
    pointer_pos: (f64, f64),
}

/// A tray-menu request in flight: the click landed, the dbusmenu
/// layout hasn't arrived yet.
struct PendingMenu {
    address: Address,
    /// Icon rect in the bar's surface-local logical coordinates.
    anchor: Rect,
    /// The bar surface the popup will parent to.
    parent: wl_surface::WlSurface,
    /// Click serial, for the popup grab.
    serial: u32,
}

/// The tray-menu popup (one at a time): an xdg_popup parented to a bar
/// layer surface via `zwlr_layer_surface_v1.get_popup`, with its own
/// swapchain and damascene runner.
struct MenuSurface {
    // Drop order: wgpu surface first (it borrows the wl_surface kept
    // alive by `popup`), same as BarSurface.
    wgpu_surface: wgpu::Surface<'static>,
    swapchain: Option<Swapchain>,
    popup: Popup,
    /// The parenting bar surface; the menu dies with it.
    parent: wl_surface::WlSurface,
    address: Address,
    app: MenuApp,
    /// Logical size (from the popup configure).
    width: u32,
    height: u32,
    scale: i32,
    dirty: bool,
    anim_deadline: Option<Instant>,
    pointer_pos: (f64, f64),
}

struct Bar {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    /// For tray-menu popups (xdg_popup needs an xdg_wm_base).
    xdg_shell: XdgShell,
    workspaces: WorkspacesState,
    toplevels: ToplevelsState,
    conn: Connection,
    config: Config,
    instance: wgpu::Instance,
    gpu: Option<GpuShared>,
    surfaces: Vec<BarSurface>,
    pointer: Option<wl_pointer::WlPointer>,
    sysmon: SysMon,
    /// Right-cluster module specs shared with every BarApp.
    modules: Rc<Vec<Module>>,
    appearance: Rc<Appearance>,
    /// SNI host thread handle; present while the config has a tray
    /// module. Dropping it shuts the thread down.
    tray: Option<Tray>,
    /// Event channel into the calloop (kept for tray restarts).
    tray_send: calloop_channel::Sender<TrayEvent>,
    /// Latest tray snapshot, fanned out to every BarApp per draw.
    tray_items: Vec<TrayItem>,
    /// Menu click waiting on its dbusmenu layout.
    pending_menu: Option<PendingMenu>,
    /// The open tray menu, if any.
    menu: Option<MenuSurface>,
    /// Seat + serial of the last button press, for popup grabs.
    seat: Option<wl_seat::WlSeat>,
    last_press_serial: u32,
    has_clock: bool,
    /// Debounced config-reload deadline (armed by the inotify source).
    reload_at: Option<Instant>,
    /// Global-state redraw flag (protocol events); fanned out to every
    /// surface's `dirty` in the main loop.
    dirty: bool,
    /// Unix second of the last clock-driven redraw.
    last_clock_secs: i64,
    exit: bool,
}

impl Bar {
    /// Push the config's geometry onto a layer surface (used at
    /// creation and on live reload). The caller commits.
    fn apply_layer_geometry(config: &Config, layer: &LayerSurface) {
        let height = config.height;
        let margin = config.margin;
        match config.position {
            Position::Top => {
                layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
                layer.set_margin(margin, margin, 0, margin);
            }
            Position::Bottom => {
                layer.set_anchor(Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
                layer.set_margin(0, margin, margin, margin);
            }
        }
        layer.set_size(0, height);
        // The compositor adds the anchored-edge margin to the zone.
        layer.set_exclusive_zone(height as i32);
    }

    fn create_bar(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput, name: String) {
        tracing::info!(output = %name, "creating bar");
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Top,
            Some("prism-bar"),
            Some(&output),
        );
        Self::apply_layer_geometry(&self.config, &layer);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();

        // SAFETY: the wl_display and wl_surface pointers stay valid for
        // the life of this BarSurface — `conn` is owned by `Bar`, the
        // wl_surface by `layer`, and `wgpu_surface` drops first (field
        // order).
        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(self.conn.backend().display_ptr() as *mut _).expect("display ptr"),
        ));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(layer.wl_surface().id().as_ptr() as *mut _).expect("surface ptr"),
        ));
        let wgpu_surface = unsafe {
            self.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(raw_display),
                    raw_window_handle: raw_window,
                })
        }
        .expect("create wgpu surface on layer surface");

        if self.gpu.is_none() {
            let adapter =
                pollster::block_on(self.instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::default(),
                    compatible_surface: Some(&wgpu_surface),
                    force_fallback_adapter: false,
                }))
                .expect("no compatible adapter");
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("prism-bar::device"),
                    ..Default::default()
                }))
                .expect("request device");
            tracing::info!(backend = ?adapter.get_info().backend, "gpu initialized");
            self.gpu = Some(GpuShared {
                adapter,
                device,
                queue,
            });
        }

        self.surfaces.push(BarSurface {
            wgpu_surface,
            swapchain: None,
            layer,
            output,
            output_name: name,
            app: BarApp::new(self.modules.clone(), self.appearance.clone()),
            width: 0,
            height: self.config.height,
            scale: 1,
            dirty: false,
            anim_deadline: None,
            pointer_pos: (0.0, 0.0),
        });
    }

    /// Start or stop the tray thread to match the configured modules.
    fn ensure_tray(&mut self) {
        let wanted = self.modules.iter().any(|m| matches!(m, Module::Tray(_)));
        match (&self.tray, wanted) {
            (None, true) => self.tray = Some(Tray::spawn(self.tray_send.clone())),
            (Some(_), false) => {
                self.tray = None;
                self.tray_items.clear();
            }
            _ => {}
        }
    }

    fn on_tray_event(&mut self, qh: &QueueHandle<Self>, event: TrayEvent) {
        match event {
            TrayEvent::Items(items) => {
                self.tray_items = items;
                self.dirty = true;
            }
            TrayEvent::Menu { address, root } => {
                let Some(pending) = self.pending_menu.take_if(|p| p.address == address) else {
                    return; // stale reply (icon clicked again, config reloaded…)
                };
                self.open_menu(qh, pending, root);
            }
            TrayEvent::MenuError { address } => {
                self.pending_menu.take_if(|p| p.address == address);
            }
        }
    }

    /// Map the tray menu as an xdg_popup parented to the bar's layer
    /// surface (`zwlr_layer_surface_v1.get_popup`), grabbed on the
    /// triggering click so the compositor dismisses it on outside
    /// clicks (`popup_done` → [`PopupHandler::done`]).
    fn open_menu(&mut self, qh: &QueueHandle<Self>, pending: PendingMenu, root: MenuNode) {
        self.close_menu();
        let Some(parent_i) = self.surface_index_for(&pending.parent) else {
            return; // bar surface gone while the layout was in flight
        };

        let app = MenuApp::new(self.appearance.clone(), root);
        let (mw, mh) = app.desired_size();
        let (mw, mh) = (mw.ceil() as i32, mh.ceil() as i32);

        let positioner = match XdgPositioner::new(&self.xdg_shell) {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(%err, "xdg_positioner");
                return;
            }
        };
        positioner.set_size(mw, mh);
        let a = pending.anchor;
        positioner.set_anchor_rect(
            a.x.floor() as i32,
            a.y.floor() as i32,
            (a.w.ceil() as i32).max(1),
            (a.h.ceil() as i32).max(1),
        );
        // Drop away from the bar's screen edge; flip if it doesn't fit.
        match self.config.position {
            Position::Top => {
                positioner.set_anchor(XdgAnchor::Bottom);
                positioner.set_gravity(Gravity::Bottom);
            }
            Position::Bottom => {
                positioner.set_anchor(XdgAnchor::Top);
                positioner.set_gravity(Gravity::Top);
            }
        }
        positioner
            .set_constraint_adjustment(ConstraintAdjustment::SlideX | ConstraintAdjustment::FlipY);

        let surface = self.compositor.create_surface(qh);
        let popup = match Popup::from_surface(None, &positioner, qh, surface, &self.xdg_shell) {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(%err, "xdg_popup");
                return;
            }
        };
        let parent = &self.surfaces[parent_i];
        parent.layer.get_popup(popup.xdg_popup());
        if let Some(seat) = &self.seat {
            popup.xdg_popup().grab(seat, pending.serial);
        }
        let scale = parent.scale;
        popup.wl_surface().set_buffer_scale(scale);
        popup.wl_surface().commit();

        // SAFETY: same invariant as in create_bar — `conn` is owned by
        // `Bar`, the wl_surface by `popup`, and `wgpu_surface` drops
        // first (MenuSurface field order).
        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(self.conn.backend().display_ptr() as *mut _).expect("display ptr"),
        ));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(popup.wl_surface().id().as_ptr() as *mut _).expect("surface ptr"),
        ));
        let wgpu_surface = unsafe {
            self.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(raw_display),
                    raw_window_handle: raw_window,
                })
        }
        .expect("create wgpu surface on popup");

        self.menu = Some(MenuSurface {
            wgpu_surface,
            swapchain: None,
            popup,
            parent: pending.parent,
            address: pending.address,
            app,
            width: mw as u32,
            height: mh as u32,
            scale,
            dirty: false,
            anim_deadline: None,
            pointer_pos: (0.0, 0.0),
        });
    }

    /// Tear the menu down, telling the item's dbusmenu it closed.
    fn close_menu(&mut self) {
        if let Some(menu) = self.menu.take() {
            if let Some(tray) = &self.tray {
                tray.send(TrayCmd::MenuClosed(menu.address.clone()));
            }
        }
    }

    /// Close the menu if its parenting bar surface is gone (output
    /// unplugged, config reload dropped the bar).
    fn drop_orphaned_menu(&mut self) {
        if let Some(m) = &self.menu {
            if self.surface_index_for(&m.parent).is_none() {
                self.close_menu();
            }
        }
    }

    fn configure_menu_swapchain(&mut self) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        let Some(m) = self.menu.as_mut() else {
            return;
        };
        let scale = m.scale as u32;
        let (w, h) = ((m.width * scale).max(1), (m.height * scale).max(1));
        let theme = m.app.theme();
        setup_swapchain(
            gpu,
            &m.wgpu_surface,
            &mut m.swapchain,
            (w, h),
            theme,
            "menu",
        );
    }

    fn draw_menu(&mut self) {
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        let Some(m) = self.menu.as_mut() else {
            return;
        };
        m.dirty = false;
        let Some(sc) = m.swapchain.as_mut() else {
            return; // not configured yet; the configure will redraw
        };
        let outcome = render_frame(
            gpu,
            &m.wgpu_surface,
            sc,
            &mut m.app,
            (m.width, m.height),
            m.scale,
            "menu",
        );
        m.dirty = outcome.retry;
        m.anim_deadline = outcome.anim_deadline;
    }

    /// Pointer event on the menu popup's surface.
    fn menu_pointer_event(&mut self, event: &PointerEvent) {
        let Some(m) = self.menu.as_mut() else {
            return;
        };
        let Some(sc) = m.swapchain.as_mut() else {
            return;
        };
        let (x, y) = (event.position.0 as f32, event.position.1 as f32);
        let events = match event.kind {
            PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                m.pointer_pos = event.position;
                sc.runner.pointer_moved(Pointer::moving(x, y)).events
            }
            PointerEventKind::Leave { .. } => sc.runner.pointer_left(),
            PointerEventKind::Press { button, .. } | PointerEventKind::Release { button, .. } => {
                let Some(button) = linux_button(button) else {
                    return;
                };
                let p = Pointer::mouse(m.pointer_pos.0 as f32, m.pointer_pos.1 as f32, button);
                if matches!(event.kind, PointerEventKind::Press { .. }) {
                    sc.runner.pointer_down(p)
                } else {
                    sc.runner.pointer_up(p)
                }
            }
            PointerEventKind::Axis { .. } => return,
        };
        m.dirty = true;
        let cx = damascene_core::EventCx::new().with_ui_state(sc.runner.ui_state());
        for e in events {
            m.app.on_event(e, &cx);
        }
        // A leaf click both reports to the app and closes the menu.
        let clicked = m.app.take_clicked().map(|id| (m.address.clone(), id));
        if let Some((address, id)) = clicked {
            if let Some(tray) = &self.tray {
                tray.send(TrayCmd::MenuClicked { address, id });
            }
            self.close_menu();
        }
    }

    /// Reload the config file and apply the differences live. A file
    /// that fails to load keeps the running config.
    fn reload_config(&mut self, qh: &QueueHandle<Self>) {
        let new = match Config::load() {
            Ok(c) => c,
            Err(err) => {
                tracing::error!("config reload failed; keeping current config\n{err:#}");
                return;
            }
        };
        tracing::info!("config reloaded");
        self.config = new;
        self.modules = Rc::new(self.config.modules());
        self.appearance = Rc::new(self.config.appearance());
        self.has_clock = self.modules.iter().any(|m| matches!(m, Module::Clock(_)));
        self.sysmon = SysMon::new(&self.modules, self.config.sample_interval);
        self.ensure_tray();
        // The menu renders stale appearance/items after a reload; the
        // user can reopen it.
        self.close_menu();
        self.pending_menu = None;

        // Drop bars on outputs the new config no longer wants.
        self.surfaces.retain(|s| {
            let keep = self.config.wants_output(&s.output_name);
            if !keep {
                tracing::info!(output = %s.output_name, "bar removed by config");
            }
            keep
        });
        // Re-apply geometry + modules to surviving bars.
        for s in &mut self.surfaces {
            Self::apply_layer_geometry(&self.config, &s.layer);
            s.layer.commit();
            s.height = self.config.height;
            s.app = BarApp::new(self.modules.clone(), self.appearance.clone());
            s.dirty = true;
        }
        // Create bars on newly wanted outputs.
        let outputs: Vec<_> = self.output_state.outputs().collect();
        for output in outputs {
            let Some(name) = self.output_state.info(&output).and_then(|i| i.name) else {
                continue;
            };
            if self.config.wants_output(&name) && !self.surfaces.iter().any(|s| s.output == output)
            {
                self.create_bar(qh, output, name);
            }
        }
    }

    /// Configure (or reconfigure) the swapchain for surface `i` from its
    /// current logical size + scale.
    fn configure_swapchain(&mut self, i: usize) {
        let gpu = self.gpu.as_ref().expect("gpu exists once surfaces do");
        let s = &mut self.surfaces[i];
        let scale = s.scale as u32;
        let (w, h) = ((s.width * scale).max(1), (s.height * scale).max(1));
        let theme = s.app.theme();
        setup_swapchain(
            gpu,
            &s.wgpu_surface,
            &mut s.swapchain,
            (w, h),
            theme,
            &s.output_name,
        );
    }

    fn draw(&mut self, i: usize) {
        // Per-output workspace strip and title: each bar describes its
        // own display.
        let ws = self.workspaces.snapshot(Some(&self.surfaces[i].output));
        let title = self.toplevels.focused_title(&self.surfaces[i].output);

        let gpu = self.gpu.as_ref().expect("gpu exists once surfaces do");
        let s = &mut self.surfaces[i];
        s.dirty = false;
        let Some(sc) = s.swapchain.as_mut() else {
            return; // not configured yet; the configure will redraw
        };

        s.app.set_state(
            ws,
            title,
            self.sysmon.stats.clone(),
            self.tray_items.clone(),
        );
        let outcome = render_frame(
            gpu,
            &s.wgpu_surface,
            sc,
            &mut s.app,
            (s.width, s.height),
            s.scale,
            &s.output_name,
        );
        s.dirty = outcome.retry;
        s.anim_deadline = outcome.anim_deadline;
    }

    fn surface_index_for(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|s| s.layer.wl_surface() == surface)
    }

    fn dispatch_ui_events(&mut self, i: usize, events: Vec<damascene_core::UiEvent>) {
        if events.is_empty() {
            return;
        }
        let s = &mut self.surfaces[i];
        // Geometry queries answer from the runner's last laid-out frame;
        // events only arrive from an already-configured runner, but degrade
        // to an empty context rather than unwrap if that ever changes.
        let cx = match &s.swapchain {
            Some(sc) => damascene_core::EventCx::new().with_ui_state(sc.runner.ui_state()),
            None => damascene_core::EventCx::new(),
        };
        for event in events {
            s.app.on_event(event, &cx);
        }
        // Side effects the app requested (it can't talk wayland itself).
        if let Some(slot) = s.app.take_activate() {
            self.workspaces.activate(slot);
        }
        if let Some(action) = s.app.take_tray_action() {
            match action {
                TrayAction::Cmd(cmd) => {
                    if let Some(tray) = &self.tray {
                        tray.send(cmd);
                    }
                }
                TrayAction::OpenMenu { address, anchor } => {
                    // Remember the click context; the popup maps when
                    // the dbusmenu layout arrives (TrayEvent::Menu).
                    self.close_menu();
                    self.pending_menu = Some(PendingMenu {
                        address: address.clone(),
                        anchor,
                        parent: self.surfaces[i].layer.wl_surface().clone(),
                        serial: self.last_press_serial,
                    });
                    if let Some(tray) = &self.tray {
                        tray.send(TrayCmd::MenuOpen(address));
                    }
                }
            }
        }
        self.surfaces[i].dirty = true;
    }
}

impl LayerShellHandler for Bar {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor dismissed this surface (e.g. output going
        // away); drop the bar but keep running for hotplug.
        self.surfaces
            .retain(|s| s.layer.wl_surface() != layer.wl_surface());
        self.drop_orphaned_menu();
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(i) = self.surface_index_for(layer.wl_surface()) else {
            return;
        };
        let (w, h) = configure.new_size;
        {
            let s = &mut self.surfaces[i];
            if w > 0 {
                s.width = w;
            }
            if h > 0 {
                s.height = h;
            }
        }
        self.configure_swapchain(i);
        self.surfaces[i].dirty = true;
    }
}

impl PopupHandler for Bar {
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        popup: &Popup,
        config: PopupConfigure,
    ) {
        if !self.menu.as_ref().is_some_and(|m| m.popup == *popup) {
            return;
        }
        {
            let m = self.menu.as_mut().expect("checked above");
            if config.width > 0 {
                m.width = config.width as u32;
            }
            if config.height > 0 {
                m.height = config.height as u32;
            }
        }
        self.configure_menu_swapchain();
        if let Some(m) = self.menu.as_mut() {
            m.dirty = true;
        }
    }

    fn done(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, popup: &Popup) {
        // Compositor dismissed the popup (outside click broke the grab).
        if self.menu.as_ref().is_some_and(|m| m.popup == *popup) {
            self.close_menu();
        }
    }
}

impl CompositorHandler for Bar {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if self
            .menu
            .as_ref()
            .is_some_and(|m| m.popup.wl_surface() == surface)
        {
            let m = self.menu.as_mut().expect("checked above");
            if m.scale != new_factor {
                m.scale = new_factor;
                surface.set_buffer_scale(new_factor);
                if m.swapchain.is_some() {
                    self.configure_menu_swapchain();
                }
                if let Some(m) = self.menu.as_mut() {
                    m.dirty = true;
                }
            }
            return;
        }
        let Some(i) = self.surface_index_for(surface) else {
            return;
        };
        if self.surfaces[i].scale != new_factor {
            self.surfaces[i].scale = new_factor;
            surface.set_buffer_scale(new_factor);
            if self.surfaces[i].swapchain.is_some() {
                self.configure_swapchain(i);
            }
            self.surfaces[i].dirty = true;
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Bar {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let Some(name) = self.output_state.info(&output).and_then(|i| i.name) else {
            tracing::warn!("output without a name; skipping");
            return;
        };
        if !self.config.wants_output(&name) {
            tracing::debug!(output = %name, "not configured for a bar; skipping");
            return;
        }
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }
        self.create_bar(qh, output, name);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        // Mode/scale changes arrive as layer configure + per-surface
        // scale_factor_changed; nothing to do here.
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let before = self.surfaces.len();
        self.surfaces.retain(|s| s.output != output);
        if self.surfaces.len() != before {
            tracing::info!("output gone; bar removed");
        }
        self.drop_orphaned_menu();
    }
}

impl SeatHandler for Bar {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            self.seat = Some(seat);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Bar {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            // Grab serials come from button presses on any surface.
            if let PointerEventKind::Press { serial, .. } = event.kind {
                self.last_press_serial = serial;
            }
            if self
                .menu
                .as_ref()
                .is_some_and(|m| m.popup.wl_surface() == &event.surface)
            {
                self.menu_pointer_event(event);
                continue;
            }
            let Some(i) = self.surface_index_for(&event.surface) else {
                continue;
            };
            // SCTK positions are surface-local logical coordinates —
            // exactly what damascene's pointer methods take.
            let (x, y) = (event.position.0 as f32, event.position.1 as f32);
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.surfaces[i].pointer_pos = event.position;
                    let Some(sc) = self.surfaces[i].swapchain.as_mut() else {
                        continue;
                    };
                    let moved = sc.runner.pointer_moved(Pointer::moving(x, y));
                    let needs_redraw = moved.needs_redraw;
                    self.dispatch_ui_events(i, moved.events);
                    if needs_redraw {
                        self.surfaces[i].dirty = true;
                    }
                }
                PointerEventKind::Leave { .. } => {
                    let Some(sc) = self.surfaces[i].swapchain.as_mut() else {
                        continue;
                    };
                    let events = sc.runner.pointer_left();
                    self.dispatch_ui_events(i, events);
                    self.surfaces[i].dirty = true;
                }
                PointerEventKind::Press { button, .. }
                | PointerEventKind::Release { button, .. } => {
                    let Some(button) = linux_button(button) else {
                        continue;
                    };
                    let (px, py) = (
                        self.surfaces[i].pointer_pos.0 as f32,
                        self.surfaces[i].pointer_pos.1 as f32,
                    );
                    let Some(sc) = self.surfaces[i].swapchain.as_mut() else {
                        continue;
                    };
                    let p = Pointer::mouse(px, py, button);
                    let events = if matches!(event.kind, PointerEventKind::Press { .. }) {
                        sc.runner.pointer_down(p)
                    } else {
                        sc.runner.pointer_up(p)
                    };
                    self.dispatch_ui_events(i, events);
                    self.surfaces[i].dirty = true;
                }
                PointerEventKind::Axis { .. } => {}
            }
        }
    }
}

/// Watch the config file's parent directory for changes to the file
/// and arm `Bar::reload_at` (debounced — editors emit event bursts,
/// and rename-replace saves never touch the watched fd of the file
/// itself, hence the directory watch). No config directory yet means
/// live reload stays inactive for this run.
fn watch_config(event_loop: &mut EventLoop<Bar>) -> Result<()> {
    use rustix::fs::inotify;

    let Some(path) = Config::path() else {
        return Ok(());
    };
    let (Some(dir), Some(file_name)) = (path.parent(), path.file_name()) else {
        return Ok(());
    };
    if !dir.is_dir() {
        tracing::info!("{} absent; live config reload inactive", dir.display());
        return Ok(());
    }
    let file_name = file_name.to_owned();

    let fd = inotify::init(inotify::CreateFlags::NONBLOCK | inotify::CreateFlags::CLOEXEC)
        .context("inotify init")?;
    inotify::add_watch(
        &fd,
        dir,
        inotify::WatchFlags::CLOSE_WRITE
            | inotify::WatchFlags::MOVED_TO
            | inotify::WatchFlags::CREATE
            | inotify::WatchFlags::DELETE,
    )
    .context("inotify add_watch")?;
    tracing::debug!("watching {} for config changes", dir.display());

    event_loop
        .handle()
        .insert_source(
            Generic::new(fd, Interest::READ, Mode::Level),
            move |_, fd, bar: &mut Bar| {
                let mut buf = [std::mem::MaybeUninit::uninit(); 1024];
                let mut reader = inotify::Reader::new(fd, &mut buf);
                while let Ok(event) = reader.next() {
                    let matches = event
                        .file_name()
                        .map(|n| n.to_bytes() == file_name.as_encoded_bytes())
                        .unwrap_or(false);
                    if matches {
                        bar.reload_at = Some(Instant::now() + Duration::from_millis(150));
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("insert inotify source: {e}"))?;
    Ok(())
}

fn linux_button(code: u32) -> Option<PointerButton> {
    match code {
        0x110 => Some(PointerButton::Primary),   // BTN_LEFT
        0x111 => Some(PointerButton::Secondary), // BTN_RIGHT
        0x112 => Some(PointerButton::Middle),    // BTN_MIDDLE
        _ => None,
    }
}

impl ProvidesRegistryState for Bar {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Bar);
delegate_output!(Bar);
delegate_layer!(Bar);
// Not delegate_xdg_shell!: that macro also wires the xdg-decoration
// objects, whose SCTK dispatch impls demand a WindowHandler. Popups
// only need the wm_base; the decoration manager (bound as part of
// XdgShell::bind) has no events, so a manual no-op dispatch suffices.
wayland_client::delegate_dispatch!(Bar: [
    wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase: smithay_client_toolkit::globals::GlobalData
] => XdgShell);
impl
    wayland_client::Dispatch<
        wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        smithay_client_toolkit::globals::GlobalData,
    > for Bar
{
    fn event(
        _: &mut Self,
        _: &wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        _: wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::Event,
        _: &smithay_client_toolkit::globals::GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!("zxdg_decoration_manager_v1 has no events");
    }
}
delegate_xdg_popup!(Bar);
delegate_seat!(Bar);
delegate_pointer!(Bar);
delegate_registry!(Bar);
