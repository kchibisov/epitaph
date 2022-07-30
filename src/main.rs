use std::error::Error;
use std::ops::Mul;
use std::process;
use std::time::{Duration, Instant};

use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle};
use smithay::backend::egl::context::GlAttributes;
use smithay::backend::egl::native::{EGLNativeDisplay, EGLPlatform};
use smithay::backend::egl::{self, ffi as egl_ffi, ffi};
use smithay::egl_platform;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::event_loop::WaylandSource;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::protocol::wl_display::WlDisplay;
use smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::protocol::wl_touch::WlTouch;
use smithay_client_toolkit::reexports::client::{Connection, EventQueue, Proxy, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::touch::TouchHandler;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::layer::{
    LayerHandler, LayerState, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_seat,
    delegate_touch, registry_handlers,
};

use crate::drawer::Drawer;
use crate::panel::Panel;
use crate::renderer::Renderer;

mod drawer;
mod module;
mod panel;
mod renderer;
mod text;
mod vertex;

mod gl {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

/// Attributes for OpenGL context creation.
pub const GL_ATTRIBUTES: GlAttributes =
    GlAttributes { version: (2, 0), profile: None, debug: false, vsync: false };

/// Maximum time between redraws.
const FRAME_INTERVAL: Duration = Duration::from_secs(60);

/// Time between drawer animation updates.
const ANIMATION_INTERVAL: Duration = Duration::from_millis(1000 / 120);

/// Height percentage when drawer animation starts opening instead
/// of closing.
const ANIMATION_THRESHOLD: f64 = 0.25;

/// Step size for drawer animation.
const ANIMATION_STEP: f64 = 20.;

/// Percentage of height reserved at bottom of drawer for closing it.
const DRAWER_CLOSE_PERCENTAGE: f64 = 0.95;

fn main() {
    // Initialize Wayland connection.
    let mut connection = match Connection::connect_to_env() {
        Ok(connection) => connection,
        Err(err) => {
            eprintln!("Error: {}", err);
            process::exit(1);
        },
    };
    let mut queue = connection.new_event_queue();

    // Setup calloop event loop.
    let mut event_loop = EventLoop::try_new().expect("event loop creation");

    // Setup shared state.
    let mut state =
        State::new(&mut connection, &mut queue, event_loop.handle()).expect("state setup");

    // Insert wayland into calloop loop.
    let wayland_source = WaylandSource::new(queue).expect("wayland source creation");
    wayland_source.insert(event_loop.handle()).expect("wayland source registration");

    // Start event loop.
    let mut next_frame = Instant::now() + FRAME_INTERVAL;
    while !state.terminated {
        // Calculate upper bound for event queue dispatch timeout.
        let timeout = next_frame.saturating_duration_since(Instant::now());

        // Dispatch Wayland & Calloop event queue.
        event_loop.dispatch(Some(timeout), &mut state).expect("event dispatch");

        // Request redraw when `FRAME_INTERVAL` was reached.
        let now = Instant::now();
        if now >= next_frame {
            next_frame = now + FRAME_INTERVAL;

            state.drawer().request_frame();
            state.panel().request_frame();
        }
    }
}

/// Wayland protocol handler state.
pub struct State {
    event_loop: LoopHandle<'static, Self>,
    protocol_states: ProtocolStates,
    active_touch: Option<i32>,
    drawer_opening: bool,
    drawer_offset: f64,
    terminated: bool,

    touch: Option<WlTouch>,
    drawer: Option<Drawer>,
    panel: Option<Panel>,
}

impl State {
    fn new(
        connection: &mut Connection,
        queue: &mut EventQueue<Self>,
        event_loop: LoopHandle<'static, Self>,
    ) -> Result<Self, Box<dyn Error>> {
        // Setup globals.
        let queue_handle = queue.handle();
        let protocol_states = ProtocolStates::new(connection, &queue_handle);

        let mut state = Self {
            protocol_states,
            event_loop,
            drawer_opening: Default::default(),
            drawer_offset: Default::default(),
            active_touch: Default::default(),
            terminated: Default::default(),
            drawer: Default::default(),
            touch: Default::default(),
            panel: Default::default(),
        };

        // Roundtrip to initialize globals.
        queue.blocking_dispatch(&mut state)?;
        queue.blocking_dispatch(&mut state)?;

        state.init_windows(connection, queue)?;

        Ok(state)
    }

    /// Initialize the panel/drawer windows and their EGL surfaces.
    fn init_windows(
        &mut self,
        connection: &mut Connection,
        queue: &EventQueue<Self>,
    ) -> Result<(), Box<dyn Error>> {
        // Setup OpenGL symbol loader.
        unsafe {
            egl_ffi::make_sure_egl_is_loaded()?;
            gl::load_with(|symbol| egl::get_proc_address(symbol));
        }

        // Setup panel window.
        self.panel = Some(Panel::new(
            connection,
            &self.protocol_states.compositor,
            queue.handle(),
            &mut self.protocol_states.layer,
        )?);

        // Setup drawer window.
        self.drawer = Some(Drawer::new(connection, queue.handle())?);

        Ok(())
    }

    fn drawer(&mut self) -> &mut Drawer {
        self.drawer.as_mut().expect("Drawer window access before initialization")
    }

    fn panel(&mut self) -> &mut Panel {
        self.panel.as_mut().expect("Panel window access before initialization")
    }
}

impl ProvidesRegistryState for State {
    registry_handlers![CompositorState, OutputState, LayerState, SeatState];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.protocol_states.registry
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.protocol_states.compositor
    }

    fn scale_factor_changed(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        surface: &WlSurface,
        factor: i32,
    ) {
        if self.panel().owns_surface(surface) {
            self.panel().set_scale_factor(factor);
        } else if self.drawer().owns_surface(surface) {
            self.drawer().set_scale_factor(factor);
        }
    }

    fn frame(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        surface: &WlSurface,
        _time: u32,
    ) {
        if self.panel().owns_surface(surface) {
            if let Err(error) = self.panel().draw() {
                eprintln!("Panel rendering failed: {:?}", error);
            }
        } else if self.drawer().owns_surface(surface) {
            let offset = self.drawer_offset;
            if let Err(error) = self.drawer().draw(offset) {
                eprintln!("Drawer rendering failed: {:?}", error);
            }
        }
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.protocol_states.output
    }

    fn new_output(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _output: WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _output: WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _output: WlOutput,
    ) {
    }
}

impl LayerHandler for State {
    fn layer_state(&mut self) -> &mut LayerState {
        &mut self.protocol_states.layer
    }

    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.terminated = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if self.panel().owns_surface(layer.wl_surface()) {
            self.panel().reconfigure(configure);
        } else if self.drawer().owns_surface(layer.wl_surface()) {
            self.drawer().reconfigure(configure);
        }
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.protocol_states.seat
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _connection: &Connection,
        queue: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Touch && self.touch.is_none() {
            self.touch = self.protocol_states.seat.get_touch(queue, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _seat: WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Touch {
            if let Some(touch) = self.touch.take() {
                touch.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl TouchHandler for State {
    fn down(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _touch: &WlTouch,
        _serial: u32,
        _time: u32,
        surface: WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        if self.active_touch.is_none() && self.panel().owns_surface(&surface) {
            let compositor = &self.protocol_states.compositor;
            let layer_state = &mut self.protocol_states.layer;
            if let Err(err) = self.drawer.as_mut().unwrap().show(compositor, layer_state) {
                eprintln!("Error: Couldn't open drawer: {}", err);
            }

            self.drawer_offset = position.1;
            self.active_touch = Some(id);
            self.drawer_opening = true;
        } else if self.drawer().owns_surface(&surface)
            && position.1 >= self.drawer().max_offset() * DRAWER_CLOSE_PERCENTAGE
        {
            self.drawer_offset = position.1;
            self.active_touch = Some(id);
            self.drawer_opening = false;
        }
    }

    fn up(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _touch: &WlTouch,
        _serial: u32,
        _time: u32,
        id: i32,
    ) {
        if self.active_touch == Some(id) {
            self.active_touch = None;

            // Start drawer animation.
            let timer = Timer::from_duration(ANIMATION_INTERVAL);
            let _ = self.event_loop.insert_source(timer, animate_drawer);
        }
    }

    fn motion(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _touch: &WlTouch,
        _time: u32,
        id: i32,
        position: (f64, f64),
    ) {
        if self.active_touch == Some(id) {
            self.drawer_offset = position.1;
            self.drawer().request_frame();
        }
    }

    fn cancel(&mut self, _connection: &Connection, _queue: &QueueHandle<Self>, _touch: &WlTouch) {}

    fn shape(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _touch: &WlTouch,
        _id: i32,
        _major: f64,
        _minor: f64,
    ) {
    }

    fn orientation(
        &mut self,
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
        _touch: &WlTouch,
        _id: i32,
        _orientation: f64,
    ) {
    }
}

delegate_compositor!(State);
delegate_output!(State);
delegate_layer!(State);
delegate_seat!(State);
delegate_touch!(State);

delegate_registry!(State);

#[derive(Debug)]
struct ProtocolStates {
    compositor: CompositorState,
    registry: RegistryState,
    output: OutputState,
    layer: LayerState,
    seat: SeatState,
}

impl ProtocolStates {
    fn new(connection: &Connection, queue: &QueueHandle<State>) -> Self {
        Self {
            registry: RegistryState::new(connection, queue),
            compositor: CompositorState::new(),
            output: OutputState::new(),
            layer: LayerState::new(),
            seat: SeatState::new(),
        }
    }
}

#[derive(Copy, Clone, Default, Debug)]
pub struct Size<T = i32> {
    pub width: T,
    pub height: T,
}

impl<T> Size<T> {
    fn new(width: T, height: T) -> Self {
        Self { width, height }
    }
}

impl From<Size> for Size<f32> {
    fn from(from: Size) -> Self {
        Self { width: from.width as f32, height: from.height as f32 }
    }
}

impl Mul<f64> for Size {
    type Output = Self;

    fn mul(mut self, factor: f64) -> Self {
        self.width = (self.width as f64 * factor) as i32;
        self.height = (self.height as f64 * factor) as i32;
        self
    }
}

struct NativeDisplay {
    display: WlDisplay,
}

impl NativeDisplay {
    fn new(display: WlDisplay) -> Self {
        Self { display }
    }
}

impl EGLNativeDisplay for NativeDisplay {
    fn supported_platforms(&self) -> Vec<EGLPlatform<'_>> {
        let display = self.display.id().as_ptr();
        vec![
            egl_platform!(PLATFORM_WAYLAND_KHR, display, &["EGL_KHR_platform_wayland"]),
            egl_platform!(PLATFORM_WAYLAND_EXT, display, &["EGL_EXT_platform_wayland"]),
        ]
    }
}

/// Drawer animation frame.
fn animate_drawer(now: Instant, _: &mut (), state: &mut State) -> TimeoutAction {
    // Compute threshold beyond which motion will automatically be completed.
    let max_offset = state.drawer().max_offset();
    let threshold = if state.drawer_opening {
        max_offset * ANIMATION_THRESHOLD
    } else {
        max_offset - max_offset * ANIMATION_THRESHOLD
    };

    // Update drawer position.
    if state.drawer_offset >= threshold {
        state.drawer_offset += ANIMATION_STEP;
    } else {
        state.drawer_offset -= ANIMATION_STEP;
    }

    if state.drawer_offset <= 0. {
        state.drawer().hide();

        TimeoutAction::Drop
    } else if state.drawer_offset >= state.drawer().max_offset() {
        state.drawer().request_frame();

        TimeoutAction::Drop
    } else {
        state.drawer().request_frame();

        TimeoutAction::ToInstant(now + ANIMATION_INTERVAL)
    }
}
