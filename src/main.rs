//! weyes: a Wayland-native xeyes for KDE Plasma.

mod core;
mod pointer;
mod render;
mod wayland;

use std::{
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};

use crate::{
    core::{EyeConfig, Point, Rect, Transform, compute_pupils},
    render::Colors,
};

const HELP: &str = "\
weyes: eyes that follow your mouse, natively on Wayland (KDE Plasma)

USAGE: weyes [OPTIONS]

OPTIONS:
  --biblical          the biblically accurate layout (ten eyes)
  --distance          pupils travel in proportion to how far the mouse is
                      toward the screen edge, like `xeyes -distance`
  --size WxH          initial window size in logical pixels
  --backend NAME      where to get the pointer position from:
                        auto      detect the running compositor (default)
                        kwin      KDE Plasma (script inside KWin, over D-Bus)
                        hyprland  Hyprland (polls its IPC socket)
                        manual    only expose the D-Bus service; feed it yourself
                        none      no feed; eyes follow the pointer only while
                                  it's over them (also what Sway gets, see README)
  --no-kwin           alias for --backend manual
  --snapshot FILE     render one frame to a PNG and exit (no compositor needed)
  --at X,Y            with --snapshot: mouse position in window pixels
  -h, --help          show this help

Left-drag moves the window. Right-click opens KWin's window menu.
";

struct Args {
    biblical: bool,
    distance: bool,
    backend: Option<String>,
    size: Option<(u32, u32)>,
    snapshot: Option<PathBuf>,
    at: Option<(f64, f64)>,
}

fn parse_pair<T: std::str::FromStr>(s: &str, sep: char, what: &str) -> Result<(T, T)> {
    let (a, b) = s.split_once(sep).with_context(|| format!("{what} should look like A{sep}B"))?;
    Ok((
        a.trim().parse().map_err(|_| anyhow::anyhow!("bad number in {what}: {a}"))?,
        b.trim().parse().map_err(|_| anyhow::anyhow!("bad number in {what}: {b}"))?,
    ))
}

fn parse_args() -> Result<Args> {
    let mut a = Args { biblical: false, distance: false, backend: None, size: None, snapshot: None, at: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--biblical" => a.biblical = true,
            "--distance" => a.distance = true,
            "--backend" => a.backend = Some(it.next().context("--backend needs a name")?),
            "--no-kwin" => a.backend = Some("manual".into()),
            "--size" => a.size = Some(parse_pair(&it.next().context("--size needs WxH")?, 'x', "--size")?),
            "--snapshot" => a.snapshot = Some(it.next().context("--snapshot needs a file")?.into()),
            "--at" => a.at = Some(parse_pair(&it.next().context("--at needs X,Y")?, ',', "--at")?),
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            other => bail!("unknown option {other} (try --help)"),
        }
    }
    Ok(a)
}

/// Render a single frame to a PNG without touching Wayland or D-Bus.
fn snapshot(args: &Args, path: &std::path::Path) -> Result<()> {
    let cfg = EyeConfig::new(args.biblical);
    let unit = if args.biblical { 50.0 } else { 80.0 };
    let (w, h) = args.size.unwrap_or(((cfg.width() * unit).round() as u32, (cfg.height() * unit).round() as u32));

    let t = Transform::for_window(&cfg, w as f64, h as f64);
    let pupils = match args.at {
        Some((x, y)) => {
            let mouse = t.to_eye(Point { x, y });
            // Pretend the "screen" is 1920x1080 with the window in its middle.
            let screen = args.distance.then(|| {
                let origin = t.to_eye(Point { x: -(1920.0 - w as f64) / 2.0, y: -(1080.0 - h as f64) / 2.0 });
                let (sw, sh) = t.size_to_eye(1920.0, 1080.0);
                Rect { x: origin.x, y: origin.y, width: sw, height: sh }
            });
            compute_pupils(&cfg, mouse, screen)
        }
        None => cfg.eyes.to_vec(),
    };

    let pm = render::render(&cfg, &pupils, w, h, &Colors::default()).context("window size too small")?;
    pm.save_png(path).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {} ({w}x{h})", path.display());
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args()?;

    if let Some(path) = &args.snapshot {
        return snapshot(&args, path);
    }

    // A socketpair the D-Bus thread uses to wake the Wayland loop up.
    let (wake_rx, wake_tx) = UnixStream::pair()?;
    wake_rx.set_nonblocking(true)?;
    wake_tx.set_nonblocking(true)?;

    // Ctrl-C / SIGTERM: ask the loop to stop (and wake it so it notices).
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let wake = wake_tx.try_clone()?;
        ctrlc::set_handler(move || {
            stop.store(true, Ordering::Relaxed);
            let _ = std::io::Write::write(&mut &wake, &[1]);
        })?;
    }

    let shared: pointer::Shared = Default::default();

    // Pick how we learn where the pointer is (auto-detected unless --backend
    // says otherwise). Dropping `_feed` at the end of main shuts it down, e.g.
    // unloads the KWin script.
    let choice = pointer::choose(args.backend.as_deref())?;
    eprintln!("weyes: pointer backend: {}", choice.backend.name());
    if let Some(note) = choice.note {
        eprintln!("weyes: note: {note}");
    }
    let _feed = pointer::start(choice.backend, shared.clone(), wake_tx)?;

    wayland::run(
        &wayland::Options { biblical: args.biblical, distance: args.distance, size: args.size },
        shared,
        wake_rx,
        stop,
    )
}
