//! Getting the pointer position on Wayland.
//!
//! A Wayland client only hears about the pointer while it is over the client's
//! own surface, and can't learn where its own window is on screen. xeyes gets
//! around this on X11 by listening to root-window events; there is no
//! equivalent here, by design. So each compositor has to tell us, in its own
//! way, and each way is a *backend*:
//!
//! | backend    | how                                                        |
//! |------------|------------------------------------------------------------|
//! | `kwin`     | a script inside KWin pushes updates to us over D-Bus       |
//! | `hyprland` | we poll Hyprland's IPC socket (`cursorpos`, `clients`)     |
//! | `manual`   | we just expose the D-Bus service; something else feeds it  |
//! | `none`     | no feed; eyes only follow the pointer while it's over them |
//!
//! Every backend does the same two things: store the newest [`Feed`] in the
//! [`Shared`] slot, and poke the wake-up socket so the Wayland loop notices.

mod dbus;
mod hyprland;
mod kwin;

use std::{
    any::Any,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
};

use anyhow::{Result, bail};

/// The `xdg_toplevel` app_id. Backends use it to find our window.
pub const APP_ID: &str = "weyes";

/// One snapshot from the compositor, in global logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Feed {
    pub cursor: (i32, i32),
    /// Top-left of our window, if the compositor has told us.
    pub window_origin: Option<(i32, i32)>,
    /// Bounding box of all outputs as (x, y, width, height).
    pub screen: Option<(i32, i32, i32, i32)>,
}

impl Feed {
    /// Parse the manual/KWin wire format `"cx cy wx wy ww wh sx sy sw sh"`.
    pub fn parse(s: &str) -> Option<Feed> {
        let v: Vec<i32> = s.split_whitespace().map(|n| n.parse().ok()).collect::<Option<_>>()?;
        if v.len() != 10 {
            return None;
        }
        Some(Feed {
            cursor: (v[0], v[1]),
            window_origin: (v[4] > 0 && v[5] > 0).then_some((v[2], v[3])),
            screen: (v[8] > 0 && v[9] > 0).then_some((v[6], v[7], v[8], v[9])),
        })
    }
}

/// The latest feed, written by a backend thread and taken by the main loop.
pub type Shared = Arc<Mutex<Option<Feed>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Kwin,
    Hyprland,
    Manual,
    None,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Kwin => "kwin",
            Backend::Hyprland => "hyprland",
            Backend::Manual => "manual",
            Backend::None => "none",
        }
    }
}

/// A chosen backend, plus an explanation to show the user when it's a
/// fallback.
#[derive(Debug, PartialEq, Eq)]
pub struct Choice {
    pub backend: Backend,
    pub note: Option<&'static str>,
}

const SWAY_NOTE: &str = "Sway has no way to report the global pointer position (no IPC for it), \
so the eyes will only follow the pointer while it is over them. Use `--backend manual` \
to feed positions yourself.";
const UNKNOWN_NOTE: &str = "couldn't recognise your compositor, so the eyes will only follow the \
pointer while it is over them. If it's KDE Plasma or Hyprland, try `--backend kwin` or \
`--backend hyprland`.";

/// Pick a backend from the environment. `get` looks up an environment
/// variable, which keeps this testable without touching the real one.
pub fn detect_with(get: impl Fn(&str) -> Option<String>) -> Choice {
    let is_set = |k: &str| get(k).is_some_and(|v| !v.is_empty());

    // Compositor-specific variables first: they're set only by that compositor.
    if is_set("HYPRLAND_INSTANCE_SIGNATURE") {
        return Choice { backend: Backend::Hyprland, note: None };
    }
    if is_set("SWAYSOCK") {
        return Choice { backend: Backend::None, note: Some(SWAY_NOTE) };
    }
    // KDE sets XDG_CURRENT_DESKTOP=KDE (possibly colon-separated with others).
    let desktop = get("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_uppercase();
    if desktop.split(':').any(|d| d == "KDE") || get("KDE_FULL_SESSION").as_deref() == Some("true") {
        return Choice { backend: Backend::Kwin, note: None };
    }
    Choice { backend: Backend::None, note: Some(UNKNOWN_NOTE) }
}

/// Resolve the `--backend` argument (None or "auto" means detect).
pub fn choose(arg: Option<&str>) -> Result<Choice> {
    let detect = || detect_with(|k| std::env::var(k).ok());
    Ok(match arg.map(str::to_ascii_lowercase).as_deref() {
        None | Some("auto") => detect(),
        Some("kwin" | "kde" | "plasma") => Choice { backend: Backend::Kwin, note: None },
        Some("hyprland") => Choice { backend: Backend::Hyprland, note: None },
        Some("manual" | "dbus") => Choice { backend: Backend::Manual, note: None },
        Some("none") => Choice { backend: Backend::None, note: None },
        Some("sway") => Choice { backend: Backend::None, note: Some(SWAY_NOTE) },
        Some(other) => bail!("unknown backend {other:?} (expected auto, kwin, hyprland, manual or none)"),
    })
}

/// Keeps a backend alive. Dropping it shuts the backend down (stops threads,
/// unloads the KWin script, releases the D-Bus name).
pub struct Guard {
    _inner: Box<dyn Any>,
}

pub fn start(backend: Backend, shared: Shared, wake: UnixStream) -> Result<Guard> {
    let inner: Box<dyn Any> = match backend {
        Backend::Kwin => Box::new(kwin::KwinFeed::start(shared, wake)?),
        Backend::Hyprland => Box::new(hyprland::HyprlandFeed::start(shared, wake)?),
        Backend::Manual => Box::new(dbus::serve(shared, wake)?),
        Backend::None => Box::new(()),
    };
    Ok(Guard { _inner: inner })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn parses_a_full_update() {
        let f = Feed::parse("100 200 50 60 300 150 0 0 1920 1080").unwrap();
        assert_eq!(f.cursor, (100, 200));
        assert_eq!(f.window_origin, Some((50, 60)));
        assert_eq!(f.screen, Some((0, 0, 1920, 1080)));
    }

    #[test]
    fn window_not_found_yet() {
        let f = Feed::parse("100 200 0 0 0 0 0 0 1920 1080").unwrap();
        assert_eq!(f.window_origin, None);
    }

    #[test]
    fn negative_coordinates_are_fine_for_multi_monitor() {
        let f = Feed::parse("-500 20 -400 10 300 150 -1920 0 3840 1080").unwrap();
        assert_eq!(f.cursor, (-500, 20));
        assert_eq!(f.screen, Some((-1920, 0, 3840, 1080)));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(Feed::parse("").is_none());
        assert!(Feed::parse("1 2 3").is_none());
        assert!(Feed::parse("a b c d e f g h i j").is_none());
    }

    #[test]
    fn detects_hyprland() {
        let c = detect_with(env(&[("HYPRLAND_INSTANCE_SIGNATURE", "abc_123"), ("XDG_CURRENT_DESKTOP", "Hyprland")]));
        assert_eq!(c, Choice { backend: Backend::Hyprland, note: None });
    }

    #[test]
    fn detects_kde_even_with_other_desktops_listed() {
        let c = detect_with(env(&[("XDG_CURRENT_DESKTOP", "KDE")]));
        assert_eq!(c.backend, Backend::Kwin);
        let c = detect_with(env(&[("XDG_CURRENT_DESKTOP", "ubuntu:KDE")]));
        assert_eq!(c.backend, Backend::Kwin);
        let c = detect_with(env(&[("KDE_FULL_SESSION", "true")]));
        assert_eq!(c.backend, Backend::Kwin);
    }

    #[test]
    fn sway_falls_back_to_hover_only_with_an_explanation() {
        let c = detect_with(env(&[("SWAYSOCK", "/run/user/1000/sway-ipc.sock"), ("XDG_CURRENT_DESKTOP", "sway")]));
        assert_eq!(c.backend, Backend::None);
        assert!(c.note.unwrap().contains("Sway"));
    }

    #[test]
    fn empty_variables_do_not_count_as_set() {
        let c = detect_with(env(&[("HYPRLAND_INSTANCE_SIGNATURE", ""), ("SWAYSOCK", "")]));
        assert_eq!(c.backend, Backend::None);
    }

    #[test]
    fn unknown_compositor_is_hover_only() {
        let c = detect_with(env(&[("XDG_CURRENT_DESKTOP", "GNOME")]));
        assert_eq!(c.backend, Backend::None);
        assert!(c.note.is_some());
    }

    #[test]
    fn explicit_backend_overrides_detection() {
        assert_eq!(choose(Some("hyprland")).unwrap().backend, Backend::Hyprland);
        assert_eq!(choose(Some("KDE")).unwrap().backend, Backend::Kwin);
        assert_eq!(choose(Some("dbus")).unwrap().backend, Backend::Manual);
        assert!(choose(Some("mutter")).is_err());
    }
}
