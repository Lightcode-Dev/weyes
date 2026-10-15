//! The `dev.weyes.Eyes` D-Bus service: a single `Update(s)` method that stores
//! the newest [`Feed`] and wakes the main loop.
//!
//! The KWin backend's script calls it, and the `manual` backend exposes it so
//! you can feed positions from anything that can talk D-Bus:
//!
//!     busctl --user call dev.weyes.Eyes /dev/weyes/Eyes dev.weyes.Eyes \
//!         Update s "cx cy wx wy ww wh sx sy sw sh"

use std::{io::Write, os::unix::net::UnixStream};

use anyhow::{Context, Result};

use super::{Feed, Shared};

pub const SERVICE: &str = "dev.weyes.Eyes";
pub const OBJECT_PATH: &str = "/dev/weyes/Eyes";

struct Service {
    shared: Shared,
    wake: UnixStream,
}

#[zbus::interface(name = "dev.weyes.Eyes")]
impl Service {
    fn update(&self, payload: &str) {
        let Some(feed) = Feed::parse(payload) else { return };
        *self.shared.lock().unwrap() = Some(feed);
        // Poke the main loop awake. The socket is non-blocking; if it's full
        // the loop is already going to wake up, so dropping the byte is fine.
        let _ = (&self.wake).write(&[1]);
    }
}

/// Claim our bus name and serve `Update`. Keep the returned connection alive
/// for as long as the service should run.
pub fn serve(shared: Shared, wake: UnixStream) -> Result<zbus::blocking::Connection> {
    let claim = || format!("claiming {SERVICE} (is weyes already running?)");
    zbus::blocking::connection::Builder::session()
        .context("connecting to the D-Bus session bus")?
        .name(SERVICE)
        .with_context(claim)?
        .serve_at(OBJECT_PATH, Service { shared, wake })?
        .build()
        .with_context(claim)
}
