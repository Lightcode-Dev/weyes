//! The Wayland side: one transparent `xdg_toplevel`, drawn with `wl_shm`
//! buffers, fed pointer positions by `pointer.rs`.

use std::{
    io::Read,
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use memmap2::MmapMut;
use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{MemfdFlags, ftruncate, memfd_create},
    io::Errno,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_pointer::{self, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::{
    wp::{
        fractional_scale::v1::client::{
            wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
            wp_fractional_scale_v1::{self, WpFractionalScaleV1},
        },
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::shell::client::{
        xdg_surface::{self, XdgSurface},
        xdg_toplevel::{self, XdgToplevel},
        xdg_wm_base::{self, XdgWmBase},
    },
};

use crate::{
    core::{EyeConfig, Point, Rect, Transform, compute_pupils},
    pointer::{APP_ID, Feed, Shared},
    render::{self, Colors},
};

/// Linux evdev button codes, as delivered by `wl_pointer.button`.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

pub struct Options {
    pub biblical: bool,
    pub distance: bool,
    pub size: Option<(u32, u32)>,
}

/// One shared-memory buffer. `busy` is flipped back to false by the
/// compositor's `wl_buffer.release` event.
struct Buf {
    buffer: WlBuffer,
    map: MmapMut,
    width: u32,
    height: u32,
    busy: Arc<AtomicBool>,
}

impl Drop for Buf {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

struct App {
    // Protocol objects we keep using after setup.
    shm: WlShm,
    surface: WlSurface,
    toplevel: XdgToplevel,
    seat: Option<WlSeat>, // absent on a compositor with no input devices
    viewport: Option<WpViewport>,
    _fractional: Option<WpFractionalScaleV1>,
    pointer: Option<WlPointer>,

    // Window state.
    size: (u32, u32),         // logical size, as negotiated with the compositor
    pending_size: Option<(u32, u32)>,
    configured: bool,
    scale120: u32,            // fractional scale in 1/120ths (120 = 1.0x)

    // What to draw.
    cfg: EyeConfig,
    colors: Colors,
    distance: bool,
    feed: Option<Feed>,
    hover: Option<Point>,     // pointer position while over our own window
    pupils: Vec<Point>,
    last_px: Vec<(i32, i32)>,

    // Drawing machinery.
    buffers: Vec<Buf>,
    frame_pending: bool,
    dirty: bool,

    // Input bookkeeping.
    pointer_pos: (f64, f64),

    running: bool,
}

impl App {
    fn buffer_size(&self) -> (u32, u32) {
        // Only scale the buffer if we can tell the compositor the logical size
        // via wp_viewporter; otherwise stay at 1x and let it scale for us.
        let s = if self.viewport.is_some() { self.scale120 as f64 / 120.0 } else { 1.0 };
        (
            ((self.size.0 as f64 * s).round() as u32).max(1),
            ((self.size.1 as f64 * s).round() as u32).max(1),
        )
    }

    /// Recompute where the pupils should be. Returns true if that moves any
    /// pupil by at least a buffer pixel (i.e. a redraw would show a change).
    fn update_pupils(&mut self) -> bool {
        let (lw, lh) = (self.size.0 as f64, self.size.1 as f64);
        let t = Transform::for_window(&self.cfg, lw, lh);

        let mut mouse: Option<Point> = None;
        let mut screen: Option<Rect> = None;

        // Preferred source: the compositor feed, once it knows where our
        // window is. Fall back to our own pointer events while hovering.
        if let Some(f) = self.feed {
            if let Some((wx, wy)) = f.window_origin {
                let local = Point { x: (f.cursor.0 - wx) as f64, y: (f.cursor.1 - wy) as f64 };
                mouse = Some(t.to_eye(local));

                if self.distance {
                    screen = f.screen.map(|(sx, sy, sw, sh)| {
                        let origin = t.to_eye(Point { x: (sx - wx) as f64, y: (sy - wy) as f64 });
                        let (w, h) = t.size_to_eye(sw as f64, sh as f64);
                        Rect { x: origin.x, y: origin.y, width: w, height: h }
                    });
                }
            }
        }
        if mouse.is_none() {
            mouse = self.hover.map(|p| t.to_eye(p));
        }

        self.pupils = match mouse {
            Some(m) => compute_pupils(&self.cfg, m, screen),
            // No idea where the pointer is: eyes look straight ahead.
            None => self.cfg.eyes.to_vec(),
        };

        let (bw, bh) = self.buffer_size();
        let px = render::pupil_pixels(&self.cfg, &self.pupils, bw, bh);
        let changed = px != self.last_px;
        self.last_px = px;
        changed
    }

    fn request_redraw(&mut self, qh: &QueueHandle<Self>) {
        self.dirty = true;
        if !self.frame_pending {
            self.draw(qh);
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if !self.configured {
            return;
        }
        let (bw, bh) = self.buffer_size();

        // Drop buffers of the wrong size as soon as the compositor is done
        // with them.
        self.buffers.retain(|b| (b.width, b.height) == (bw, bh) || b.busy.load(Ordering::Acquire));

        let idx = match self
            .buffers
            .iter()
            .position(|b| (b.width, b.height) == (bw, bh) && !b.busy.load(Ordering::Acquire))
        {
            Some(i) => i,
            None => match create_buffer(&self.shm, bw, bh, qh) {
                Ok(b) => {
                    self.buffers.push(b);
                    self.buffers.len() - 1
                }
                Err(e) => {
                    eprintln!("weyes: couldn't allocate a {bw}x{bh} buffer: {e:#}");
                    return;
                }
            },
        };

        let Some(pm) = render::render(&self.cfg, &self.pupils, bw, bh, &self.colors) else {
            return;
        };
        let buf = &mut self.buffers[idx];
        render::copy_to_argb8888(&pm, &mut buf.map);
        buf.busy.store(true, Ordering::Release);

        if let Some(vp) = &self.viewport {
            vp.set_destination(self.size.0 as i32, self.size.1 as i32);
        }
        self.surface.attach(Some(&buf.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, bw as i32, bh as i32);
        self.surface.frame(qh, ());
        self.surface.commit();

        self.frame_pending = true;
        self.dirty = false;
    }
}

fn create_buffer(shm: &WlShm, width: u32, height: u32, qh: &QueueHandle<App>) -> Result<Buf> {
    let stride = width * 4;
    let size = (stride * height) as usize;

    // An anonymous in-memory file, shared with the compositor via the pool.
    let fd = memfd_create("weyes-shm", MemfdFlags::CLOEXEC).context("memfd_create")?;
    ftruncate(&fd, size as u64).context("ftruncate")?;
    let file = std::fs::File::from(fd);
    // SAFETY: the file is private to us and the compositor only reads it.
    let map = unsafe { MmapMut::map_mut(&file) }.context("mmap")?;

    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let busy = Arc::new(AtomicBool::new(false));
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, wl_shm::Format::Argb8888, qh, busy.clone());
    // The buffer keeps the underlying memory alive; the pool object isn't needed.
    pool.destroy();

    Ok(Buf { buffer, map, width, height, busy })
}

// --- Event handlers ---------------------------------------------------------

impl Dispatch<WlRegistry, GlobalListContents> for App {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as wayland_client::Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(App: WlCompositor);
delegate_noop!(App: ignore WlShm);
delegate_noop!(App: WlShmPool);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: WpViewporter);
delegate_noop!(App: WpViewport);
delegate_noop!(App: WpFractionalScaleManagerV1);

impl Dispatch<WlBuffer, Arc<AtomicBool>> for App {
    fn event(_: &mut Self, _: &WlBuffer, event: wl_buffer::Event, busy: &Arc<AtomicBool>, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Release);
        }
    }
}

impl Dispatch<WlCallback, ()> for App {
    fn event(state: &mut Self, _: &WlCallback, event: wl_callback::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_pending = false;
            if state.dirty {
                state.draw(qh);
            }
        }
    }
}

impl Dispatch<XdgWmBase, ()> for App {
    fn event(_: &mut Self, base: &XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for App {
    fn event(state: &mut Self, xdg: &XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            if let Some(size) = state.pending_size.take() {
                state.size = size;
            }
            state.configured = true;
            state.update_pupils();
            state.request_redraw(qh);
        }
    }
}

impl Dispatch<XdgToplevel, ()> for App {
    fn event(state: &mut Self, _: &XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                // 0x0 means "you pick"; keep whatever we have.
                if width > 0 && height > 0 {
                    state.pending_size = Some((width as u32, height as u32));
                }
            }
            xdg_toplevel::Event::Close => state.running = false,
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for App {
    fn event(state: &mut Self, _: &WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            if state.scale120 != scale {
                state.scale120 = scale;
                state.update_pupils();
                state.request_redraw(qh);
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(state: &mut Self, seat: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for App {
    fn event(state: &mut Self, _: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } | wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.pointer_pos = (surface_x, surface_y);
                state.hover = Some(Point { x: surface_x, y: surface_y });
                if state.update_pupils() {
                    state.request_redraw(qh);
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.hover = None;
                if state.update_pupils() {
                    state.request_redraw(qh);
                }
            }
            wl_pointer::Event::Button { serial, button, state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => {
                let Some(seat) = &state.seat else { return };
                match button {
                    // Drag to move: hand the grab to the compositor.
                    BTN_LEFT => state.toplevel._move(seat, serial),
                    // Right-click: KWin's own window menu (move/resize/close/...).
                    BTN_RIGHT => {
                        let (x, y) = state.pointer_pos;
                        state.toplevel.show_window_menu(seat, serial, x as i32, y as i32);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

// --- Setup and the event loop ----------------------------------------------

pub fn run(opts: &Options, shared: Shared, wake: UnixStream, stop: Arc<AtomicBool>) -> Result<()> {
    let conn = Connection::connect_to_env().context("connecting to the Wayland compositor (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn).context("reading the Wayland registry")?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals.bind(&qh, 1..=6, ()).context("wl_compositor")?;
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).context("wl_shm")?;
    let wm_base: XdgWmBase = globals.bind(&qh, 1..=6, ()).context("xdg_wm_base (xdg-shell)")?;
    // Optional: without a seat we just lose drag-to-move and the right-click menu.
    let seat: Option<WlSeat> = globals.bind(&qh, 1..=7, ()).ok();
    // Optional: crisp rendering on fractionally-scaled outputs.
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let fractional_mgr: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("weyes".into());
    // KWin matches our window by this in the pointer script.
    toplevel.set_app_id(APP_ID.into());

    let viewport = viewporter.as_ref().map(|v| v.get_viewport(&surface, &qh, ()));
    let fractional = match (&fractional_mgr, &viewport) {
        (Some(m), Some(_)) => Some(m.get_fractional_scale(&surface, &qh, ())),
        _ => None,
    };

    let cfg = EyeConfig::new(opts.biblical);
    let unit = if opts.biblical { 50.0 } else { 80.0 };
    let size = opts.size.unwrap_or(((cfg.width() * unit).round() as u32, (cfg.height() * unit).round() as u32));

    let mut app = App {
        shm,
        surface: surface.clone(),
        toplevel,
        seat,
        viewport,
        _fractional: fractional,
        pointer: None,
        size,
        pending_size: None,
        configured: false,
        scale120: 120,
        cfg,
        colors: Colors::default(),
        distance: opts.distance,
        feed: None,
        hover: None,
        pupils: Vec::new(),
        last_px: Vec::new(),
        buffers: Vec::new(),
        frame_pending: false,
        dirty: false,
        pointer_pos: (0.0, 0.0),
        running: true,
    };

    // The initial commit (no buffer) asks the compositor to send us a
    // configure; we draw our first frame in response to it.
    surface.commit();

    while app.running && !stop.load(Ordering::Relaxed) {
        queue.dispatch_pending(&mut app)?;

        // Drain the wake-up channel, then pick up the newest compositor feed.
        let mut sink = [0u8; 64];
        while matches!((&wake).read(&mut sink), Ok(n) if n > 0) {}
        if let Some(feed) = shared.lock().unwrap().take() {
            app.feed = Some(feed);
            if app.configured && app.update_pupils() {
                app.request_redraw(&qh);
            }
        }

        if !app.running || stop.load(Ordering::Relaxed) {
            break;
        }

        match queue.flush() {
            Ok(()) => {}
            Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => bail!("flushing Wayland requests: {e}"),
        }

        // Standard prepare_read dance: if events are already queued, go
        // dispatch them instead of blocking.
        let Some(guard) = queue.prepare_read() else { continue };

        let wayland_ready = {
            let wl_fd = guard.connection_fd();
            let mut fds = [PollFd::new(&wl_fd, PollFlags::IN), PollFd::new(&wake, PollFlags::IN)];
            match poll(&mut fds, None) {
                Ok(_) | Err(Errno::INTR) => {}
                Err(e) => bail!("poll: {e}"),
            }
            fds[0].revents().contains(PollFlags::IN)
        };

        if wayland_ready {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => bail!("reading Wayland events: {e}"),
            }
        } else {
            drop(guard);
        }
    }

    Ok(())
}
