//! Hyprland backend.
//!
//! Hyprland has no push notification for pointer motion, but its IPC socket
//! answers `cursorpos` (global layout coordinates) instantly, so we poll it
//! from a background thread. Our own window's position comes from `clients`,
//! and the screen size (for `--distance`) from `monitors`.
//!
//! The wire protocol is: open a fresh connection to
//! `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`, write one
//! command (a `j/` prefix asks for JSON), read until the server closes it.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::{APP_ID, Feed, Shared};

/// How often to poll while the pointer is moving (about 60 Hz).
const ACTIVE_EVERY: Duration = Duration::from_millis(16);
/// How often to poll once the pointer has been still for a while.
const IDLE_EVERY: Duration = Duration::from_millis(100);
/// "Still for a while".
const IDLE_AFTER: Duration = Duration::from_secs(1);
/// How often to re-read window and monitor geometry. It changes rarely
/// (window drags, monitor hotplug), and `clients` is a much bigger reply than
/// `cursorpos`.
const GEOMETRY_EVERY: Duration = Duration::from_millis(100);
/// Don't let a wedged compositor hang us.
const IO_TIMEOUT: Duration = Duration::from_millis(500);

pub struct HyprlandFeed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HyprlandFeed {
    pub fn start(shared: Shared, wake: UnixStream) -> Result<HyprlandFeed> {
        let socket = socket_path()?;
        // Fail up front, with a useful message, if the socket isn't answering.
        let reply = request(&socket, "j/cursorpos").with_context(|| format!("talking to Hyprland at {}", socket.display()))?;
        parse_cursor(&reply).context("Hyprland's cursorpos reply wasn't what we expected")?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("weyes-hyprland".into())
                .spawn(move || poll_loop(&socket, &shared, &wake, &stop))
                .context("spawning the Hyprland polling thread")?
        };
        Ok(HyprlandFeed { stop, thread: Some(thread) })
    }
}

impl Drop for HyprlandFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn poll_loop(socket: &Path, shared: &Shared, wake: &UnixStream, stop: &AtomicBool) {
    let mut window_origin: Option<(i32, i32)> = None;
    let mut screen: Option<(i32, i32, i32, i32)> = None;
    let mut last_geometry = Instant::now() - GEOMETRY_EVERY;
    let mut last_feed: Option<Feed> = None;
    let mut last_motion = Instant::now();
    let mut complained = false;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();

        match request(socket, "j/cursorpos").and_then(|r| parse_cursor(&r)) {
            Ok(cursor) => {
                complained = false;

                if now.duration_since(last_geometry) >= GEOMETRY_EVERY {
                    last_geometry = now;
                    match request(socket, "j/clients").and_then(|r| parse_window_origin(&r)) {
                        Ok(o) => window_origin = o,
                        Err(e) => eprintln!("weyes: reading Hyprland's window list: {e:#}"),
                    }
                    match request(socket, "j/monitors").and_then(|r| parse_screen(&r)) {
                        Ok(s) => screen = s,
                        Err(e) => eprintln!("weyes: reading Hyprland's monitor list: {e:#}"),
                    }
                }

                let feed = Feed { cursor, window_origin, screen };
                if last_feed != Some(feed) {
                    if last_feed.map(|f| f.cursor) != Some(cursor) {
                        last_motion = now;
                    }
                    last_feed = Some(feed);
                    *shared.lock().unwrap() = Some(feed);
                    let _ = (&*wake).write(&[1]);
                }
            }
            Err(e) => {
                // The compositor may be restarting; keep trying, but only
                // complain once per outage.
                if !complained {
                    eprintln!("weyes: lost contact with Hyprland: {e:#}");
                    complained = true;
                }
            }
        }

        let pause = if now.duration_since(last_motion) > IDLE_AFTER { IDLE_EVERY } else { ACTIVE_EVERY };
        std::thread::sleep(pause);
    }
}

fn socket_path() -> Result<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .context("HYPRLAND_INSTANCE_SIGNATURE isn't set (are you inside a Hyprland session?)")?;

    // Current Hyprland keeps its sockets under the runtime dir; older
    // versions used /tmp/hypr.
    let mut candidates = Vec::new();
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(runtime).join("hypr").join(&sig).join(".socket.sock"));
    }
    candidates.push(PathBuf::from("/tmp/hypr").join(&sig).join(".socket.sock"));

    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    bail!("couldn't find Hyprland's IPC socket for instance {sig} (looked in {candidates:?})")
}

/// One request, one fresh connection, read to EOF.
fn request(socket: &Path, command: &str) -> Result<String> {
    let mut s = UnixStream::connect(socket).context("connecting")?;
    s.set_read_timeout(Some(IO_TIMEOUT))?;
    s.set_write_timeout(Some(IO_TIMEOUT))?;
    s.write_all(command.as_bytes()).context("sending request")?;
    let mut out = String::new();
    s.read_to_string(&mut out).context("reading reply")?;
    Ok(out)
}

// --- Parsing (pure, so it's unit-tested without a compositor) --------------

#[derive(Deserialize)]
struct CursorPos {
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
struct Client {
    #[serde(default)]
    class: String,
    #[serde(default, rename = "initialClass")]
    initial_class: String,
    // Assume mapped if the field is missing.
    #[serde(default = "yes")]
    mapped: bool,
    #[serde(default)]
    at: [f64; 2],
    #[serde(default)]
    size: [f64; 2],
}

#[derive(Deserialize)]
struct Monitor {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    #[serde(default = "one")]
    scale: f64,
}

fn yes() -> bool {
    true
}
fn one() -> f64 {
    1.0
}

fn parse_cursor(json: &str) -> Result<(i32, i32)> {
    let c: CursorPos = serde_json::from_str(json.trim()).context("parsing cursorpos JSON")?;
    Ok((c.x.round() as i32, c.y.round() as i32))
}

/// Top-left of our window in global layout coordinates, if it's open.
fn parse_window_origin(json: &str) -> Result<Option<(i32, i32)>> {
    let clients: Vec<Client> = serde_json::from_str(json.trim()).context("parsing clients JSON")?;
    let ours = |name: &str| name.eq_ignore_ascii_case(APP_ID);
    Ok(clients
        .iter()
        .find(|c| c.mapped && (ours(&c.class) || ours(&c.initial_class)) && c.size[0] > 0.0 && c.size[1] > 0.0)
        .map(|c| (c.at[0].round() as i32, c.at[1].round() as i32)))
}

/// Bounding box of all monitors in *logical* pixels (x, y, width, height).
///
/// Hyprland reports each monitor's size in physical pixels, so divide by the
/// scale. This ignores rotated monitors; it only affects how `--distance`
/// scales the pupils' travel, not where they point.
fn parse_screen(json: &str) -> Result<Option<(i32, i32, i32, i32)>> {
    let monitors: Vec<Monitor> = serde_json::from_str(json.trim()).context("parsing monitors JSON")?;
    let rects = monitors.iter().map(|m| {
        let s = if m.scale > 0.0 { m.scale } else { 1.0 };
        (m.x, m.y, m.x + m.width / s, m.y + m.height / s)
    });
    let union = rects.reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)));
    Ok(union.map(|(x0, y0, x1, y1)| (x0.round() as i32, y0.round() as i32, (x1 - x0).round() as i32, (y1 - y0).round() as i32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_reply() {
        assert_eq!(parse_cursor("{\"x\": 1024, \"y\": 768}\n").unwrap(), (1024, 768));
        // Fractional and negative (multi-monitor) positions are fine too.
        assert_eq!(parse_cursor(r#"{"x": -12.6, "y": 5.4}"#).unwrap(), (-13, 5));
        assert!(parse_cursor("Invalid").is_err());
    }

    // A trimmed-down but realistic `j/clients` reply: extra fields we don't
    // use must be ignored.
    const CLIENTS: &str = r#"[
      {"address":"0x55","mapped":true,"hidden":false,"at":[0,0],"size":[1920,1080],
       "workspace":{"id":1,"name":"1"},"floating":false,"class":"kitty","title":"~",
       "initialClass":"kitty","pid":100},
      {"address":"0x56","mapped":true,"hidden":false,"at":[600,300],"size":[304,144],
       "workspace":{"id":1,"name":"1"},"floating":true,"class":"weyes","title":"weyes",
       "initialClass":"weyes","pid":200}
    ]"#;

    #[test]
    fn finds_our_window_among_others() {
        assert_eq!(parse_window_origin(CLIENTS).unwrap(), Some((600, 300)));
    }

    #[test]
    fn no_window_yet() {
        assert_eq!(parse_window_origin("[]").unwrap(), None);
        let only_other = r#"[{"mapped":true,"at":[1,2],"size":[3,4],"class":"firefox","initialClass":"firefox"}]"#;
        assert_eq!(parse_window_origin(only_other).unwrap(), None);
    }

    #[test]
    fn unmapped_or_zero_sized_windows_are_ignored() {
        let unmapped = r#"[{"mapped":false,"at":[5,5],"size":[304,144],"class":"weyes"}]"#;
        assert_eq!(parse_window_origin(unmapped).unwrap(), None);
        let zero = r#"[{"mapped":true,"at":[5,5],"size":[0,0],"class":"weyes"}]"#;
        assert_eq!(parse_window_origin(zero).unwrap(), None);
    }

    #[test]
    fn matches_class_case_insensitively_and_via_initial_class() {
        let j = r#"[{"mapped":true,"at":[7,8],"size":[10,10],"class":"","initialClass":"WEYES"}]"#;
        assert_eq!(parse_window_origin(j).unwrap(), Some((7, 8)));
    }

    #[test]
    fn single_monitor_screen() {
        let j = r#"[{"id":0,"name":"DP-1","width":1920,"height":1080,"x":0,"y":0,"scale":1.0}]"#;
        assert_eq!(parse_screen(j).unwrap(), Some((0, 0, 1920, 1080)));
    }

    #[test]
    fn two_monitors_with_scaling_are_unioned_in_logical_pixels() {
        // A 4K monitor at 2x (3840x2160 physical = 1920x1080 logical) with a
        // 1080p monitor to its left at 1x.
        let j = r#"[
          {"width":3840,"height":2160,"x":0,"y":0,"scale":2.0},
          {"width":1920,"height":1080,"x":-1920,"y":0,"scale":1.0}
        ]"#;
        assert_eq!(parse_screen(j).unwrap(), Some((-1920, 0, 3840, 1080)));
    }

    #[test]
    fn no_monitors() {
        assert_eq!(parse_screen("[]").unwrap(), None);
    }
}
