//! prism-bar host: a wlr-layer-shell surface driven by the damascene
//! wgpu `Runner` (the custom-host path — `damascene-winit-wgpu` only
//! creates regular toplevels, so we own the surface and event loop).
//!
//! Shape of the loop:
//!
//!   SCTK layer surface (Top, anchored, exclusive zone)
//!     → wgpu Surface via raw wayland handles
//!     → per frame: app.build() → runner.prepare() → runner.render()
//!   calloop: wayland socket + redraw deadlines (animation, clock)
//!   SCTK pointer events → runner.pointer_*() → app.on_event()

mod toplevels;
mod ui;
mod workspaces;

use std::ptr::NonNull;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
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

use crate::toplevels::ToplevelsState;
use crate::ui::BarApp;
use crate::workspaces::WorkspacesState;

/// Bar height / floating margin in logical pixels. Will come from config.
const BAR_HEIGHT: u32 = 40;
const BAR_MARGIN: i32 = 6;
const MSAA_SAMPLES: u32 = 4;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "prism_bar=info".into()),
        )
        .init();

    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, event_queue) = registry_queue_init::<Bar>(&conn).context("registry init")?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1")?;

    // Layer surface: top layer, anchored to the top edge spanning the
    // full output width, reserving its height so tiled windows don't
    // overlap. Compositor picks the output (None) for the spike;
    // per-output bars come with config.
    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("prism-bar"), None);
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, BAR_HEIGHT);
    // Floating look: margins pull the surface off the screen edges; the
    // compositor adds the anchored-edge margin to the exclusive zone.
    layer.set_margin(BAR_MARGIN, BAR_MARGIN, 0, BAR_MARGIN);
    layer.set_exclusive_zone(BAR_HEIGHT as i32);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();

    let mut bar = Bar {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        workspaces: WorkspacesState::bind(&globals, &qh),
        toplevels: ToplevelsState::bind(&globals, &qh),
        conn: conn.clone(),
        layer,
        bar_output: None,
        pointer: None,
        gpu: None,
        app: BarApp::new(),
        width: 0,
        height: BAR_HEIGHT,
        scale: 1,
        dirty: false,
        anim_deadline: None,
        last_clock_secs: 0,
        pointer_pos: (0.0, 0.0),
        exit: false,
    };

    let mut event_loop: EventLoop<Bar> = EventLoop::try_new().context("calloop")?;
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {e}"))?;

    while !bar.exit {
        // Two redraw deadlines: damascene animation (`next_redraw_in`)
        // and the clock's next second boundary. Sleep until the
        // earlier; wayland events interrupt the sleep.
        let now = Instant::now();
        let clock_in = Duration::from_millis(1000 - chrono::Local::now().timestamp_subsec_millis() as u64);
        let mut timeout = clock_in;
        if let Some(d) = bar.anim_deadline {
            timeout = timeout.min(d.saturating_duration_since(now));
        }

        event_loop
            .dispatch(Some(timeout), &mut bar)
            .context("event loop dispatch")?;

        // A deadline elapsing is itself a reason to redraw.
        let now = Instant::now();
        if bar.anim_deadline.is_some_and(|d| d <= now) {
            bar.anim_deadline = None;
            bar.dirty = true;
        }
        // Clock tick: redraw when the displayed second changes.
        let secs = chrono::Local::now().timestamp();
        if secs != bar.last_clock_secs {
            bar.last_clock_secs = secs;
            bar.dirty = true;
        }

        if bar.dirty && bar.gpu.is_some() {
            bar.draw();
        }
    }
    Ok(())
}

/// GPU state, created on the first layer-surface configure (layer-shell
/// forbids attaching buffers before then, and we don't know our size).
struct Gpu {
    // Field order = drop order: surface borrows device resources and
    // (unsafely) the wl_surface, which `Bar` keeps alive.
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    runner: Runner,
    msaa: Option<MsaaTarget>,
    _instance: wgpu::Instance,
}

struct Bar {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    workspaces: WorkspacesState,
    toplevels: ToplevelsState,
    conn: Connection,
    layer: LayerSurface,
    /// The output our layer surface landed on (from surface_enter).
    bar_output: Option<wl_output::WlOutput>,
    pointer: Option<wl_pointer::WlPointer>,
    gpu: Option<Gpu>,
    app: BarApp,
    /// Logical (surface-coordinate) size.
    width: u32,
    height: u32,
    /// Integer buffer scale from the compositor.
    scale: i32,
    dirty: bool,
    /// When damascene wants the next animation frame.
    anim_deadline: Option<Instant>,
    /// Unix second of the last clock-driven redraw.
    last_clock_secs: i64,
    /// Last pointer position in logical px (button events don't carry one).
    pointer_pos: (f64, f64),
    exit: bool,
}

impl Bar {
    fn init_gpu(&mut self) {
        // SAFETY: the wl_display and wl_surface pointers stay valid for
        // the life of `Bar` — `conn` and `layer` are owned by it, and
        // `Gpu` (holding the wgpu surface) is dropped with it.
        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(self.conn.backend().display_ptr() as *mut _).expect("display ptr"),
        ));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(self.layer.wl_surface().id().as_ptr() as *mut _).expect("surface ptr"),
        ));

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(raw_display),
                raw_window_handle: raw_window,
            })
        }
        .expect("create wgpu surface on layer surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("no compatible adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("prism-bar::device"),
            ..Default::default()
        }))
        .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        // Transparent background: damascene's blend states leave correct
        // premultiplied coverage in the framebuffer over a transparent
        // clear, so PreMultiplied is the right composite mode.
        let alpha_mode = if caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            tracing::warn!(modes = ?caps.alpha_modes, "no premultiplied alpha; bar will be opaque");
            caps.alpha_modes[0]
        };
        let config = wgpu::SurfaceConfiguration {
            // COPY_SRC matches the runner's backdrop-snapshot path.
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            width: (self.width * self.scale as u32).max(1),
            height: (self.height * self.scale as u32).max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        let mut runner = Runner::with_caps(
            &device,
            &queue,
            format,
            MSAA_SAMPLES,
            RunnerCaps::from_adapter(&adapter),
        );
        runner.set_theme(self.app.theme());
        runner.set_surface_size(config.width, config.height);
        runner.warm_default_glyphs();

        let msaa = (MSAA_SAMPLES > 1).then(|| {
            MsaaTarget::new(
                &device,
                format,
                wgpu::Extent3d {
                    width: config.width,
                    height: config.height,
                    depth_or_array_layers: 1,
                },
                MSAA_SAMPLES,
            )
        });

        tracing::info!(
            backend = ?adapter.get_info().backend,
            ?format,
            "gpu initialized"
        );
        self.gpu = Some(Gpu {
            surface,
            device,
            queue,
            config,
            runner,
            msaa,
            _instance: instance,
        });
    }

    /// Apply the current logical size + scale to the swapchain.
    fn resize_gpu(&mut self) {
        let scale = self.scale as u32;
        let (w, h) = ((self.width * scale).max(1), (self.height * scale).max(1));
        let Some(gpu) = self.gpu.as_mut() else { return };
        if gpu.config.width == w && gpu.config.height == h {
            return;
        }
        gpu.config.width = w;
        gpu.config.height = h;
        gpu.surface.configure(&gpu.device, &gpu.config);
        gpu.runner.set_surface_size(w, h);
        let extent = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        if let Some(msaa) = gpu.msaa.as_mut() {
            if !msaa.matches(extent) {
                *msaa = MsaaTarget::new(&gpu.device, gpu.config.format, extent, msaa.sample_count);
            }
        }
    }

    fn draw(&mut self) {
        self.dirty = false;
        let Some(gpu) = self.gpu.as_mut() else { return };

        let scale = self.scale as f32;
        let viewport = Rect::new(0.0, 0.0, self.width as f32, self.height as f32);

        self.app.set_state(
            self.workspaces.snapshot(self.bar_output.as_ref()),
            self.toplevels.focused_title(),
        );
        self.app.before_build();
        let theme = self.app.theme();
        let mut tree = {
            let cx = BuildCx::new(&theme)
                .with_ui_state(gpu.runner.ui_state())
                .with_viewport(viewport.w, viewport.h);
            self.app.build(&cx)
        };
        gpu.runner.set_theme(theme);
        gpu.runner.set_hotkeys(self.app.hotkeys());

        let prepare = gpu
            .runner
            .prepare(&gpu.device, &gpu.queue, &mut tree, viewport, scale);

        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                self.dirty = true; // try again next loop turn
                return;
            }
            other => {
                tracing::error!("surface unavailable: {other:?}");
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
        gpu.runner.render(
            &gpu.device,
            &mut encoder,
            &frame.texture,
            &view,
            gpu.msaa.as_ref().map(|m| &m.view),
            // Transparent clear — the visible bar background is a rounded
            // rect in the tree; the compositor sees through the rest.
            wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        );
        gpu.queue.submit(Some(encoder.finish()));
        frame.present();

        self.anim_deadline = prepare.next_redraw_in.map(|d| Instant::now() + d);
        if prepare.needs_redraw && self.anim_deadline.is_none() {
            self.anim_deadline = Some(Instant::now());
        }
    }

    fn dispatch_ui_events(&mut self, events: Vec<damascene_core::UiEvent>) {
        if events.is_empty() {
            return;
        }
        for event in events {
            self.app.on_event(event);
        }
        // Side effects the app requested (it can't talk wayland itself).
        if let Some(slot) = self.app.take_activate() {
            self.workspaces.activate(slot);
        }
        self.dirty = true;
    }
}

impl LayerShellHandler for Bar {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        self.width = if w > 0 { w } else { self.width.max(1) };
        self.height = if h > 0 { h } else { BAR_HEIGHT };
        if self.gpu.is_none() {
            self.init_gpu();
        } else {
            self.resize_gpu();
        }
        self.dirty = true;
    }
}

impl CompositorHandler for Bar {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if self.scale != new_factor {
            self.scale = new_factor;
            self.layer.wl_surface().set_buffer_scale(new_factor);
            self.resize_gpu();
            self.dirty = true;
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
        output: &wl_output::WlOutput,
    ) {
        // Lets the workspace snapshot filter to this output's group.
        self.bar_output = Some(output.clone());
        self.dirty = true;
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
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
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
            if event.surface != *self.layer.wl_surface() {
                continue;
            }
            // SCTK positions are surface-local logical coordinates —
            // exactly what damascene's pointer methods take.
            let (x, y) = (event.position.0 as f32, event.position.1 as f32);
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.pointer_pos = event.position;
                    let Some(gpu) = self.gpu.as_mut() else { continue };
                    let moved = gpu.runner.pointer_moved(Pointer::moving(x, y));
                    let needs_redraw = moved.needs_redraw;
                    self.dispatch_ui_events(moved.events);
                    if needs_redraw {
                        self.dirty = true;
                    }
                }
                PointerEventKind::Leave { .. } => {
                    let Some(gpu) = self.gpu.as_mut() else { continue };
                    let events = gpu.runner.pointer_left();
                    self.dispatch_ui_events(events);
                    self.dirty = true;
                }
                PointerEventKind::Press { button, .. } | PointerEventKind::Release { button, .. } => {
                    let Some(button) = linux_button(button) else { continue };
                    let (px, py) = (self.pointer_pos.0 as f32, self.pointer_pos.1 as f32);
                    let Some(gpu) = self.gpu.as_mut() else { continue };
                    let p = Pointer::mouse(px, py, button);
                    let events = if matches!(event.kind, PointerEventKind::Press { .. }) {
                        gpu.runner.pointer_down(p)
                    } else {
                        gpu.runner.pointer_up(p)
                    };
                    self.dispatch_ui_events(events);
                    self.dirty = true;
                }
                PointerEventKind::Axis { .. } => {}
            }
        }
    }
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
