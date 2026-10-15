//! KDE Plasma backend.
//!
//! We run a tiny script *inside* KWin (`kwin/pointer.js`) that watches
//! `workspace.cursorPos` and our window's frame geometry and calls `Update` on
//! our D-Bus service (see `dbus.rs`). This module installs and removes that
//! script through KWin's `/Scripting` D-Bus interface.

use std::{os::unix::net::UnixStream, path::PathBuf};

use anyhow::{Context, Result, bail};

use super::{Shared, dbus};

const SCRIPT_NAME: &str = "weyes-pointer";
const SCRIPT_SRC: &str = include_str!("../../kwin/pointer.js");

/// Keeps the D-Bus service alive, and unloads the KWin script on drop.
pub struct KwinFeed {
    conn: zbus::blocking::Connection,
    script_loaded: bool,
}

impl KwinFeed {
    pub fn start(shared: Shared, wake: UnixStream) -> Result<KwinFeed> {
        let conn = dbus::serve(shared, wake)?;
        let mut feed = KwinFeed { conn, script_loaded: false };
        feed.load_script()?;
        Ok(feed)
    }

    fn load_script(&mut self) -> Result<()> {
        let path = script_path()?;
        std::fs::write(&path, SCRIPT_SRC).with_context(|| format!("writing {}", path.display()))?;
        let path = path.to_string_lossy().into_owned();

        // loadScript refuses a name that's already loaded (e.g. left over from
        // a crashed run), so unload first and ignore the answer.
        let _ = self.kwin_call("/Scripting", "org.kde.kwin.Scripting", "unloadScript", &(SCRIPT_NAME,));

        let reply = self
            .kwin_call("/Scripting", "org.kde.kwin.Scripting", "loadScript", &(path.as_str(), SCRIPT_NAME))
            .context("asking KWin to load the pointer script (are you running KDE Plasma?)")?;
        let id: i32 = reply.body().deserialize().context("reading loadScript reply")?;
        if id < 0 {
            bail!("KWin refused to load the pointer script (loadScript returned {id})");
        }

        // Plasma 6 exposes each loaded script at /Scripting/Script<id>.
        self.kwin_call(&format!("/Scripting/Script{id}"), "org.kde.kwin.Script", "run", &())
            .context("asking KWin to run the pointer script")?;
        self.script_loaded = true;
        Ok(())
    }

    fn kwin_call<B>(&self, path: &str, iface: &str, method: &str, body: &B) -> zbus::Result<zbus::Message>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.conn.call_method(Some("org.kde.KWin"), path, Some(iface), method, body)
    }
}

impl Drop for KwinFeed {
    fn drop(&mut self) {
        if self.script_loaded {
            let _ = self.kwin_call("/Scripting", "org.kde.kwin.Scripting", "unloadScript", &(SCRIPT_NAME,));
        }
    }
}

fn script_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("weyes");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join("pointer.js"))
}
