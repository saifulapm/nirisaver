//! The overlay itself: one layer surface per output, and the event loop that
//! feeds them.
//!
//! Three things here were learned the expensive way and are load-bearing.
//!
//! **Two buffers, not one.** A design that keeps a single reusable shm buffer
//! per output and refuses to redraw until `wl_buffer.release` arrives will
//! present exactly one frame and then stop. Nothing in the protocol promises a
//! deadline on release: a compositor that has attached your only buffer and
//! has no reason to let go of it may hold it indefinitely, and niri does.
//! Release arrives when a *different* buffer is attached, so a client with one
//! buffer deadlocks against itself. With a fade-in, the single frame it
//! managed to present is the near-transparent first one — which makes the
//! symptom a fullscreen invisible overlay holding an exclusive keyboard grab,
//! and there is no worse way for this program to fail. That is a client bug,
//! not a compositor bug, and the fix is to own two buffers and draw into
//! whichever is free.
//!
//! **Damage describes the screen, not the buffer.** See [`crate::render`].
//!
//! **The cursor goes away through the protocol.** `wl_pointer.set_cursor` with
//! a null surface is what "no cursor over my surface" means in Wayland. It
//! works on any compositor, it is scoped to this surface, and it cannot
//! outlive the process — none of which is true of asking a compositor's
//! command line to hide the cursor globally.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use smithay_client_toolkit::compositor::{
    CompositorHandler, CompositorState, FrameCallbackData, Region,
};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};

use crate::config::Settings;
use crate::engine::{Advance, Animation, Animator};
use crate::raster::{CellMetrics, Layout, Rasterizer};
use crate::render::{pixel_rect, render_frame, History, Snapshot};

/// The layer-shell namespace this program identifies itself by. It is what
/// `niri msg layers` shows and what anything scripted around the overlay
/// matches on.
pub const NAMESPACE: &str = "nirisaver";

/// Input is ignored for this long after the overlay maps.
///
/// Mapping a surface under a stationary pointer produces an enter and,
/// depending on the compositor, a motion at the position the pointer was
/// already at. Neither is anyone asking for the screensaver to go away.
const INPUT_GRACE: Duration = Duration::from_millis(250);

/// How finely a fade is stepped. The engine's own rate governs content; this
/// governs only the ramp, and only while one is running.
const FADE_STEP: Duration = Duration::from_millis(16);

/// The pixel format every buffer here uses: one native-endian `u32` per pixel,
/// colour channels premultiplied by alpha, which is what the rasterizer writes.
const FORMAT: wl_shm::Format = wl_shm::Format::Argb8888;

/// Put the screensaver on screen and run it until something dismisses it.
pub fn run(settings: &Settings) -> Result<()> {
    let signals = SignalPipe::install()?;
    let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
    let (globals, mut queue) = registry_queue_init(&conn).context("initialising the registry")?;
    let qh = queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).context("this compositor has no wl_compositor")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("this compositor does not support wlr-layer-shell")?;
    let shm = Shm::bind(&globals, &qh).context("this compositor has no wl_shm")?;

    let mut overlay = Overlay {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        settings: settings.clone(),
        surfaces: Vec::new(),
        rasterizers: HashMap::new(),
        animator: None,
        grid_size: None,
        pending_deadline: None,
        keyboard: None,
        pointer: None,
        pointer_origin: None,
        started: Instant::now(),
        dismissing: None,
        serial: 0,
        alpha: u8::MAX,
        closed: false,
        failure: None,
    };
    overlay.alpha = overlay.current_alpha();

    for output in overlay.output_state.outputs().collect::<Vec<_>>() {
        overlay.add_output(&qh, output);
    }
    // A round trip so every surface's first configure has landed before the
    // loop starts reasoning about geometry.
    queue.roundtrip(&mut overlay).context("waiting for the first configure")?;
    if let Some(failure) = overlay.failure.take() {
        return Err(failure);
    }
    if overlay.surfaces.is_empty() {
        return Err(anyhow!("no outputs to draw on"));
    }

    loop {
        overlay.tick(&qh)?;
        if overlay.finished() {
            break;
        }
        queue.flush().context("flushing the Wayland queue")?;

        let Some(read) = queue.prepare_read() else {
            queue.dispatch_pending(&mut overlay).context("dispatching Wayland events")?;
            continue;
        };
        let ready = poll_two(read.connection_fd(), signals.as_fd(), overlay.next_wakeup())?;
        if ready.signalled {
            signals.drain();
            overlay.dismiss();
        }
        if ready.wayland {
            // An error here is a compositor that went away, which is a normal
            // enough way for a screensaver to end.
            if read.read().is_err() {
                break;
            }
        } else {
            drop(read);
        }
        queue.dispatch_pending(&mut overlay).context("dispatching Wayland events")?;
    }

    match overlay.failure.take() {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}

struct SurfaceState {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    pool: SlotPool,
    /// Two buffers over two distinct slots. Each `Buffer` keeps its slot alive,
    /// so the pool cannot hand the same memory back twice — two buffers that
    /// turned out to share a slot would be one buffer with extra steps, and
    /// the deadlock this design exists to avoid would be back.
    buffers: [Buffer; 2],
    /// What each buffer currently holds.
    contents: [Option<Snapshot>; 2],
    /// What the compositor is showing, which is a different picture from
    /// either buffer once more than one is in flight.
    presented: Option<Snapshot>,
    presented_serial: u64,
    layout: Option<Layout>,
    scale: i32,
    logical: (u32, u32),
    /// Set between committing a frame and its callback arriving: at most one
    /// presentation per `wl_surface.frame`.
    awaiting_callback: bool,
    opaque_region: Option<Region>,
    opaque: bool,
}

struct Overlay {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,

    settings: Settings,
    surfaces: Vec<SurfaceState>,
    /// One rasterizer per output scale — a glyph cache is only reusable at the
    /// size it was rasterized for.
    rasterizers: HashMap<i32, Rasterizer>,
    animator: Option<Animator>,
    grid_size: Option<(usize, usize)>,
    /// How long until the animator has something to say. During a hold this is
    /// the whole hold.
    pending_deadline: Option<Duration>,

    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    pointer_origin: Option<(f64, f64)>,

    started: Instant,
    dismissing: Option<Instant>,
    /// Bumped whenever there is something new to present. Comparing an integer
    /// is what keeps an idle hold from diffing a grid on every wakeup.
    serial: u64,
    alpha: u8,
    closed: bool,
    failure: Option<anyhow::Error>,
}

impl Overlay {
    fn finished(&self) -> bool {
        if self.failure.is_some() || self.closed {
            return true;
        }
        match self.dismissing {
            // Leave once the fade-out has run *and* every surface has actually
            // presented the last frame of it: exiting early would drop the
            // overlay in one step instead of fading it.
            Some(at) => {
                at.elapsed() >= self.settings.fade_out
                    && self.surfaces.iter().all(|s| s.presented_serial == self.serial)
            }
            None => false,
        }
    }

    fn dismiss(&mut self) {
        if self.dismissing.is_none() {
            self.dismissing = Some(Instant::now());
        }
    }

    fn input_is_live(&self) -> bool {
        self.started.elapsed() >= INPUT_GRACE
    }

    fn fading(&self) -> bool {
        self.dismissing.is_some() || self.alpha != u8::MAX
    }

    /// Fade level for right now.
    fn current_alpha(&self) -> u8 {
        let ramp = |elapsed: Duration, over: Duration| -> f32 {
            if over.is_zero() {
                return 1.0;
            }
            (elapsed.as_secs_f32() / over.as_secs_f32()).clamp(0.0, 1.0)
        };
        let level = match self.dismissing {
            Some(at) => 1.0 - ramp(at.elapsed(), self.settings.fade_out),
            None => ramp(self.started.elapsed(), self.settings.fade_in),
        };
        (level * 255.0).round() as u8
    }

    /// How long the loop may sleep. `None` means "until something happens",
    /// which is what an idle hold wants: one wakeup at the end of it, not a
    /// poll timeout ticking through the whole thing.
    fn next_wakeup(&self) -> Option<Duration> {
        if self.fading() {
            return Some(FADE_STEP);
        }
        // A surface with something to present but no free buffer is waiting on
        // a release, which arrives on the Wayland socket — no timer needed.
        if self.surfaces.iter().any(|s| s.presented_serial != self.serial) {
            return None;
        }
        self.pending_deadline
    }

    fn tick(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let alpha = self.current_alpha();
        if alpha != self.alpha {
            self.alpha = alpha;
            self.serial += 1;
        }
        if self.dismissing.is_none() {
            self.advance_animation()?;
        }
        for index in 0..self.surfaces.len() {
            self.draw(qh, index);
        }
        Ok(())
    }

    fn advance_animation(&mut self) -> Result<()> {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let Some(animator) = self.animator.as_mut() else { return Ok(()) };
        match animator.advance(now_ms)? {
            Advance::Frame => {
                self.serial += 1;
                self.pending_deadline = Some(Duration::ZERO);
            }
            Advance::Idle { until_ms } => {
                self.pending_deadline =
                    Some(Duration::from_millis(until_ms.saturating_sub(now_ms)));
            }
        }
        Ok(())
    }

    fn draw(&mut self, qh: &QueueHandle<Self>, index: usize) {
        let alpha = self.alpha;
        let serial = self.serial;
        let background = self.settings.background;
        let Some(animator) = self.animator.as_ref() else { return };
        let grid = animator.grid();

        let surface = &mut self.surfaces[index];
        if surface.presented_serial == serial || surface.awaiting_callback {
            return;
        }
        let Some(layout) = surface.layout else { return };
        if layout.cols != grid.cols() || layout.rows != grid.rows() {
            return;
        }
        let Some(rasterizer) = self.rasterizers.get_mut(&surface.scale) else { return };

        // Whichever buffer the compositor is not holding. If it is holding
        // both there is genuinely nowhere to draw: skip the frame and let the
        // release wake us. Blocking here, or drawing into a buffer the
        // compositor is scanning out, are the two ways to get this wrong.
        let pool = &mut surface.pool;
        let Some(slot) = (0..2).find(|i| surface.buffers[*i].canvas(pool).is_some()) else {
            return;
        };
        let Some(canvas) = surface.buffers[slot].canvas(pool) else { return };
        // SAFETY: reinterpreting the shm mapping as `u32`. Pools are page
        // aligned and the stride is a whole number of pixels, so the split
        // below has no prefix or suffix; the guard turns a violated assumption
        // into a skipped frame rather than a scribble.
        let (prefix, pixels, suffix) = unsafe { canvas.align_to_mut::<u32>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            debug_assert!(false, "shm mapping was not u32 aligned");
            return;
        }

        let rendered = render_frame(
            pixels,
            &layout,
            rasterizer,
            grid,
            History {
                buffer: surface.contents[slot].as_ref(),
                presented: surface.presented.as_ref(),
            },
            alpha,
            background,
        );

        let wl_surface = surface.layer.wl_surface().clone();
        if rendered.full_surface {
            wl_surface.damage_buffer(0, 0, layout.width as i32, layout.height as i32);
        } else {
            for rect in &rendered.rects {
                let (x, y, w, h) = pixel_rect(&layout, *rect);
                wl_surface.damage_buffer(x, y, w, h);
            }
        }

        // Once nothing is translucent, say so: the compositor can then skip
        // blending the desktop underneath a fullscreen opaque surface. The
        // region has to come back off for a fade, or there is nothing to fade
        // against.
        let opaque = alpha == u8::MAX;
        if opaque != surface.opaque {
            if opaque {
                if surface.opaque_region.is_none() {
                    if let Ok(region) = Region::new(&self.compositor) {
                        region.add(0, 0, surface.logical.0 as i32, surface.logical.1 as i32);
                        surface.opaque_region = Some(region);
                    }
                }
                if let Some(region) = &surface.opaque_region {
                    wl_surface.set_opaque_region(Some(region.wl_region()));
                    surface.opaque = true;
                }
            } else {
                wl_surface.set_opaque_region(None);
                surface.opaque = false;
            }
        }

        if surface.buffers[slot].attach_to(&wl_surface).is_err() {
            return;
        }
        // Requested only now, with a buffer about to be committed: asking for
        // a frame callback on a surface that has never had content is asking a
        // question the compositor cannot answer.
        wl_surface.frame(qh, FrameCallbackData(wl_surface.clone()));
        wl_surface.commit();

        let snapshot = Snapshot { grid: grid.clone(), alpha };
        surface.contents[slot] = Some(snapshot.clone());
        surface.presented = Some(snapshot);
        surface.presented_serial = serial;
        surface.awaiting_callback = true;
    }

    fn add_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }
        let scale = self.output_state.info(&output).map_or(1, |i| i.scale_factor.max(1));

        let wl_surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            wl_surface,
            Layer::Overlay,
            Some(NAMESPACE),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        // Exclusive: the screensaver is the only thing that should see input
        // while it is up, and the first of it is what dismisses it.
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        // Cover everything, other layers' exclusive zones included.
        layer.set_exclusive_zone(-1);
        layer.wl_surface().set_buffer_scale(scale);
        layer.commit();

        // A one-pixel pool, resized to the real geometry on the first
        // configure. The size is not known until the compositor says so.
        let mut pool = match SlotPool::new(4, &self.shm) {
            Ok(pool) => pool,
            Err(e) => {
                self.failure = Some(anyhow!("creating a shm pool: {e}"));
                return;
            }
        };
        let buffers = match make_buffers(&mut pool, 1, 1) {
            Ok(buffers) => buffers,
            Err(e) => {
                self.failure = Some(e);
                return;
            }
        };

        self.surfaces.push(SurfaceState {
            output,
            layer,
            pool,
            buffers,
            contents: [None, None],
            presented: None,
            presented_serial: u64::MAX,
            layout: None,
            scale,
            logical: (0, 0),
            awaiting_callback: false,
            opaque_region: None,
            opaque: false,
        });
    }

    /// Rebuild a surface's buffers and layout for new geometry, then re-derive
    /// the shared grid.
    fn reconfigure(&mut self, index: usize, logical: (u32, u32)) {
        let scale = self.surfaces[index].scale;
        let font_px = self.settings.font_size * scale as f32;
        let metrics = match self.rasterizer_for(scale, font_px) {
            Ok(metrics) => metrics,
            Err(e) => {
                self.failure = Some(e);
                return;
            }
        };

        let width = logical.0 * scale as u32;
        let height = logical.1 * scale as u32;
        let stride = width as usize * 4;
        let size = stride * height as usize;

        let surface = &mut self.surfaces[index];
        surface.logical = logical;
        if surface.pool.len() < size * 2 {
            if let Err(e) = surface.pool.resize(size * 2) {
                self.failure = Some(anyhow!("resizing the shm pool: {e}"));
                return;
            }
        }
        // The old buffers describe the old geometry; drop them before asking
        // for new slots so the pool can reuse their memory.
        match make_buffers(&mut surface.pool, width as i32, height as i32) {
            Ok(buffers) => {
                surface.buffers = buffers;
                surface.contents = [None, None];
                surface.presented = None;
            }
            Err(e) => {
                self.failure = Some(e);
                return;
            }
        }
        surface.layer.wl_surface().set_buffer_scale(scale);
        surface.layout = Some(Layout::fit(width, height, metrics));
        surface.presented_serial = u64::MAX;
        surface.awaiting_callback = false;
        surface.opaque = false;
        surface.opaque_region = None;

        self.resize_grid();
    }

    fn rasterizer_for(&mut self, scale: i32, font_px: f32) -> Result<CellMetrics> {
        if let Some(existing) = self.rasterizers.get(&scale) {
            return Ok(existing.metrics());
        }
        let mut rasterizer =
            Rasterizer::new(&self.settings.font, font_px, self.settings.line_height)
                .map_err(|e| anyhow!("loading the font: {e}"))?;
        // The printable ASCII range covers a quote; the block-drawing symbols
        // effects use arrive as they are needed.
        rasterizer.warm(' '..='~');
        let metrics = rasterizer.metrics();
        self.rasterizers.insert(scale, rasterizer);
        Ok(metrics)
    }

    /// The grid every output shows.
    ///
    /// The *smallest* output decides it. A grid sized to the largest screen
    /// would have its edges cut off on the others, and a screensaver whose
    /// quote is missing its last word on the second monitor is worse than one
    /// that leaves a wider margin there.
    fn resize_grid(&mut self) {
        let mut cols = usize::MAX;
        let mut rows = usize::MAX;
        for surface in &self.surfaces {
            let Some(layout) = surface.layout else { continue };
            let fitted = Layout::fit(layout.width, layout.height, layout.cell);
            cols = cols.min(fitted.cols);
            rows = rows.min(fitted.rows);
        }
        if cols == usize::MAX || cols == 0 || rows == 0 {
            return;
        }

        for surface in &mut self.surfaces {
            if let Some(layout) = surface.layout {
                surface.layout =
                    Some(Layout::centred(layout.width, layout.height, cols, rows, layout.cell));
                surface.contents = [None, None];
                surface.presented = None;
                surface.presented_serial = u64::MAX;
            }
        }

        if self.grid_size == Some((cols, rows)) {
            return;
        }
        self.grid_size = Some((cols, rows));

        let animation = Animation {
            cols,
            rows,
            frame_rate: self.settings.frame_rate,
            hold: self.settings.hold,
            measure: self.settings.measure.clone(),
            content: self.settings.content.clone(),
            effects: self.settings.effects.clone(),
            default_fg: self.settings.foreground,
        };
        match Animator::new(animation, self.settings.seed) {
            Ok(animator) => {
                self.animator = Some(animator);
                self.serial += 1;
            }
            Err(e) => self.failure = Some(e),
        }
    }

    fn surface_index(&self, wl_surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces.iter().position(|s| s.layer.wl_surface() == wl_surface)
    }
}

/// Two buffers over two distinct slots. Both slots are alive at once, so the
/// pool cannot satisfy the second request by recycling the first.
fn make_buffers(pool: &mut SlotPool, width: i32, height: i32) -> Result<[Buffer; 2]> {
    let stride = width * 4;
    let len = (height as usize) * (stride as usize);
    let first = pool.new_slot(len).map_err(|e| anyhow!("allocating a buffer slot: {e}"))?;
    let second = pool.new_slot(len).map_err(|e| anyhow!("allocating a buffer slot: {e}"))?;
    let a = pool
        .create_buffer_in(&first, width, height, stride, FORMAT)
        .map_err(|e| anyhow!("creating a buffer: {e}"))?;
    let b = pool
        .create_buffer_in(&second, width, height, stride, FORMAT)
        .map_err(|e| anyhow!("creating a buffer: {e}"))?;
    Ok([a, b])
}

struct Ready {
    wayland: bool,
    signalled: bool,
}

fn poll_two(
    wayland: BorrowedFd<'_>,
    signals: BorrowedFd<'_>,
    timeout: Option<Duration>,
) -> Result<Ready> {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};

    let mut fds = [PollFd::new(&wayland, PollFlags::IN), PollFd::new(&signals, PollFlags::IN)];
    let spec =
        timeout.map(|d| Timespec { tv_sec: d.as_secs() as i64, tv_nsec: d.subsec_nanos() as i64 });
    loop {
        match poll(&mut fds, spec.as_ref()) {
            Ok(_) => {
                return Ok(Ready {
                    wayland: !fds[0].revents().is_empty(),
                    signalled: !fds[1].revents().is_empty(),
                })
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => Err(anyhow!("poll: {e}"))?,
        }
    }
}

/// Signals, turned into something the event loop can wait on.
///
/// A flag plus a short poll timeout would also work, and would also mean
/// waking up sixty times a second through a fourteen-second hold to ask
/// whether anything had happened. A pipe the handler writes one byte to costs
/// nothing while nothing is happening, which is the point.
struct SignalPipe {
    read: OwnedFd,
}

impl SignalPipe {
    fn install() -> Result<SignalPipe> {
        use rustix::pipe::{pipe_with, PipeFlags};
        use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

        let (read, write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)
            .context("creating the signal pipe")?;
        for signal in [SIGTERM, SIGINT, SIGHUP, SIGQUIT] {
            // One duplicate per signal: `register` takes ownership of the
            // descriptor it writes to.
            let end = write.try_clone().context("duplicating the signal pipe")?;
            signal_hook::low_level::pipe::register(signal, end)
                .with_context(|| format!("handling signal {signal}"))?;
        }
        Ok(SignalPipe { read })
    }

    fn as_fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
    }

    fn drain(&self) {
        let mut buf = [0u8; 64];
        while let Ok(n) = rustix::io::read(&self.read, &mut buf) {
            if n < buf.len() {
                break;
            }
        }
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let Some(index) = self.surface_index(surface) else { return };
        let scale = new_factor.max(1);
        if self.surfaces[index].scale == scale {
            return;
        }
        self.surfaces[index].scale = scale;
        let logical = self.surfaces[index].logical;
        if logical != (0, 0) {
            self.reconfigure(index, logical);
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if let Some(index) = self.surface_index(surface) {
            self.surfaces[index].awaiting_callback = false;
        }
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

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.add_output(qh, output);
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let Some(index) = self.surfaces.iter().position(|s| s.output == output) else { return };
        let scale = self.output_state.info(&output).map_or(1, |i| i.scale_factor.max(1));
        if self.surfaces[index].scale != scale {
            self.surfaces[index].scale = scale;
            let logical = self.surfaces[index].logical;
            if logical != (0, 0) {
                self.reconfigure(index, logical);
            }
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.surfaces.retain(|s| s.output != output);
        if self.surfaces.is_empty() {
            self.closed = true;
        } else {
            self.resize_grid();
        }
    }
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        self.surfaces.retain(|s| &s.layer != layer);
        if self.surfaces.is_empty() {
            self.closed = true;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(index) = self.surfaces.iter().position(|s| &s.layer == layer) else { return };
        let width = NonZeroU32::new(configure.new_size.0).map_or(0, NonZeroU32::get);
        let height = NonZeroU32::new(configure.new_size.1).map_or(0, NonZeroU32::get);
        if width == 0 || height == 0 {
            return;
        }
        if self.surfaces[index].logical == (width, height) && self.surfaces[index].layout.is_some()
        {
            return;
        }
        self.reconfigure(index, (width, height));
    }
}

impl SeatHandler for Overlay {
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
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            // Straight off the seat rather than through SeatState::get_keyboard,
            // which lives behind the toolkit's `xkbcommon` feature. See the
            // Dispatch impl below for why that feature is off.
            self.keyboard = Some(seat.get_keyboard(qh, ()));
        }
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
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

/// Keyboard input, straight off the wire.
///
/// The toolkit's keyboard support sits behind its `xkbcommon` feature, which
/// costs a `pkg-config` lookup for `xkbcommon` at build time and a link
/// against `libxkbcommon.so` at run time. That buys keymap compilation and
/// keysym translation — a full answer to "which character did they type".
///
/// This program's entire keyboard requirement is "did a key go down", so the
/// feature is off and `wl_keyboard` is dispatched by hand. Everything else the
/// protocol sends is dropped, the keymap file descriptor included: it arrives
/// owned, and dropping it closes it.
///
/// The saving is not theoretical. Without this, building nirisaver needs
/// `libxkbcommon-devel` (or `libxkbcommon-dev`) present, which is not a
/// dependency a screensaver should impose on a source install — and is exactly
/// what CI caught, since a stock runner has no reason to have it.
impl Dispatch<wl_keyboard::WlKeyboard, ()> for Overlay {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key { state: key_state, .. } = event {
            let pressed =
                matches!(key_state, wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed));
            if pressed && state.input_is_live() {
                state.dismiss();
            }
        }
    }
}

impl PointerHandler for Overlay {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    // The protocol's own way to say "no cursor here": scoped to
                    // this surface, and gone when the process is.
                    pointer.set_cursor(serial, None, 0, 0);
                    self.pointer_origin = Some(event.position);
                }
                PointerEventKind::Leave { .. } => self.pointer_origin = None,
                PointerEventKind::Motion { .. } => {
                    // Mapping under a stationary pointer can produce a motion
                    // at the position it was already at; a real move is a
                    // different position.
                    let moved = self.pointer_origin != Some(event.position);
                    if moved && self.input_is_live() {
                        self.dismiss();
                    }
                }
                PointerEventKind::Press { .. } if self.input_is_live() => self.dismiss(),
                _ => {}
            }
        }
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

delegate_registry!(Overlay);
delegate_dispatch2!(Overlay);
