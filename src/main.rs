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
mod sysmon;
mod toplevels;
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
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_pointer, wl_seat, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};

use damascene_core::event::{Pointer, PointerButton};
use damascene_core::prelude::{App, Rect};
use damascene_core::BuildCx;
use damascene_wgpu::{MsaaTarget, Runner, RunnerCaps};

use crate::config::{Config, Module, Position};
use crate::sysmon::SysMon;
use crate::toplevels::ToplevelsState;
use crate::ui::BarApp;
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

    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, event_queue) = registry_queue_init::<Bar>(&conn).context("registry init")?;
    let qh = event_queue.handle();

    let mut bar = Bar {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).context("wl_compositor")?,
        layer_shell: LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1")?,
        workspaces: WorkspacesState::bind(&globals, &qh),
        toplevels: ToplevelsState::bind(&globals, &qh),
        conn: conn.clone(),
        config,
        instance: wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()),
        gpu: None,
        surfaces: Vec::new(),
        pointer: None,
        sysmon: SysMon::new(&modules),
        has_clock: modules.iter().any(|m| matches!(m, Module::Clock(_))),
        modules,
        reload_at: None,
        dirty: false,
        last_clock_secs: 0,
        exit: false,
    };

    let mut event_loop: EventLoop<Bar> = EventLoop::try_new().context("calloop")?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {e}"))?;
    watch_config(&mut event_loop)?;

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
        if bar.sysmon.active()
            && Instant::now() >= bar.sysmon.next_sample
            && bar.sysmon.sample()
        {
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

        for i in 0..bar.surfaces.len() {
            if bar.surfaces[i].dirty {
                bar.draw(i);
            }
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

/// Swapchain + renderer for one bar surface; created on its first
/// layer-shell configure (before that we don't know the size).
struct Swapchain {
    config: wgpu::SurfaceConfiguration,
    msaa: Option<MsaaTarget>,
    runner: Runner,
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

struct Bar {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor: CompositorState,
    layer_shell: LayerShell,
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
            app: BarApp::new(self.modules.clone()),
            width: 0,
            height: self.config.height,
            scale: 1,
            dirty: false,
            anim_deadline: None,
            pointer_pos: (0.0, 0.0),
        });
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
        self.has_clock = self.modules.iter().any(|m| matches!(m, Module::Clock(_)));
        self.sysmon = SysMon::new(&self.modules);

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
            s.app = BarApp::new(self.modules.clone());
            s.dirty = true;
        }
        // Create bars on newly wanted outputs.
        let outputs: Vec<_> = self.output_state.outputs().collect();
        for output in outputs {
            let Some(name) = self.output_state.info(&output).and_then(|i| i.name) else {
                continue;
            };
            if self.config.wants_output(&name)
                && !self.surfaces.iter().any(|s| s.output == output)
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

        match &mut s.swapchain {
            Some(sc) => {
                if sc.config.width == w && sc.config.height == h {
                    return;
                }
                sc.config.width = w;
                sc.config.height = h;
                s.wgpu_surface.configure(&gpu.device, &sc.config);
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
                let caps = s.wgpu_surface.get_capabilities(&gpu.adapter);
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
                        output = %s.output_name,
                        modes = ?caps.alpha_modes,
                        "no premultiplied alpha; bar will be opaque"
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
                s.wgpu_surface.configure(&gpu.device, &config);

                let mut runner = Runner::with_caps(
                    &gpu.device,
                    &gpu.queue,
                    format,
                    MSAA_SAMPLES,
                    RunnerCaps::from_adapter(&gpu.adapter),
                );
                runner.set_theme(s.app.theme());
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
                tracing::info!(output = %s.output_name, ?format, "swapchain configured");
                s.swapchain = Some(Swapchain {
                    config,
                    msaa,
                    runner,
                });
            }
        }
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

        let scale = s.scale as f32;
        let viewport = Rect::new(0.0, 0.0, s.width as f32, s.height as f32);

        s.app.set_state(ws, title, self.sysmon.stats.clone());
        s.app.before_build();
        let theme = s.app.theme();
        let mut tree = {
            let cx = BuildCx::new(&theme)
                .with_ui_state(sc.runner.ui_state())
                .with_viewport(viewport.w, viewport.h);
            s.app.build(&cx)
        };
        sc.runner.set_theme(theme);
        sc.runner.set_hotkeys(s.app.hotkeys());

        let prepare = sc
            .runner
            .prepare(&gpu.device, &gpu.queue, &mut tree, viewport, scale);

        let frame = match s.wgpu_surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                s.wgpu_surface.configure(&gpu.device, &sc.config);
                s.dirty = true; // try again next loop turn
                return;
            }
            other => {
                tracing::error!(output = %s.output_name, "surface unavailable: {other:?}");
                return;
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
            // Transparent clear — the visible bar background is a rounded
            // rect in the tree; the compositor sees through the rest.
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        );
        gpu.queue.submit(Some(encoder.finish()));
        frame.present();

        s.anim_deadline = prepare.next_redraw_in.map(|d| Instant::now() + d);
        if prepare.needs_redraw && s.anim_deadline.is_none() {
            s.anim_deadline = Some(Instant::now());
        }
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
        for event in events {
            s.app.on_event(event);
        }
        // Side effects the app requested (it can't talk wayland itself).
        if let Some(slot) = s.app.take_activate() {
            self.workspaces.activate(slot);
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

impl CompositorHandler for Bar {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
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
delegate_seat!(Bar);
delegate_pointer!(Bar);
delegate_registry!(Bar);
