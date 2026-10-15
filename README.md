# weyes

A Wayland-native `xeyes`, written in Rust.

## In order to run weyes you need to git clone the repo first
    
    git clone https://github.com/Lightcode-Dev/weyes
    cd ~/git/weyes/
    
## Build and run weyes using cargo

    cargo build --release
    cargo run --release
    ./target/release/weyes              # two eyes, backend auto-detected
    ./target/release/weyes --distance   # like `xeyes -distance`
    ./target/release/weyes --biblical   # ten overlapping eyes

Left-drag moves the window. Right-click opens the compositor's window menu.

## Compositor support

Wayland won't tell a client where the pointer is outside its own window, nor
where its own window is, so each compositor needs its own *backend*:

| compositor    | backend    | how it works                                                   | status |
|---------------|------------|----------------------------------------------------------------|--------|
| KDE Plasma    | `kwin`     | a script inside KWin pushes cursor + window geometry over D-Bus | tested on real Plasma |
| Hyprland      | `hyprland` | polls Hyprland's IPC socket (`cursorpos`, `clients`, `monitors`) | tested against a fake IPC server only |
| Sway          | `none`     | Sway has no cursor-position IPC; eyes only follow the pointer while it is over them | limitation of Sway |
| anything else | `none`     | same hover-only fallback                                        | |

`--backend auto` (the default) picks from the environment:
`HYPRLAND_INSTANCE_SIGNATURE` -> Hyprland, `SWAYSOCK` -> Sway,
`XDG_CURRENT_DESKTOP` containing `KDE` -> KWin. Override with
`--backend kwin|hyprland|manual|none`.

### Why Sway is different

Sway's IPC can't report the pointer position, and its maintainers have said
that's by design: the suggested approach is a fullscreen layer-shell surface
that reads `wl_pointer.enter`, which works for a one-shot like a popup menu but
not for continuous tracking (and would disturb hover state in the windows
below). Proposals to add a `get_cursor` query and a `cursor_movement` event
exist upstream; if either lands, a `sway` backend is a small addition.

## Source layout

    src/core.rs            constants, layouts, transform, compute_pupil
    src/render.rs          software renderer + shm copy
    src/wayland.rs         window, buffers, fractional scale, input, event loop
    src/main.rs            CLI + wiring + --snapshot mode
    src/pointer/mod.rs     Feed type, backend enum, auto-detection
    src/pointer/kwin.rs    loads/unloads the KWin script
    src/pointer/dbus.rs    the dev.weyes.Eyes D-Bus service (kwin + manual)
    src/pointer/hyprland.rs  IPC polling thread
    kwin/pointer.js        the script that runs inside KWin

Adding a backend: write a type whose `start()` stores a `Feed` in the shared
slot and writes a byte to the wake-up socket; add a variant to `Backend`.

## Testing without a real compositor

    cargo test                                    # unit tests

    # render a frame to a PNG; mouse at x=600,y=40 (window pixels)
    weyes --snapshot out.png --at 600,40

    # run on any Wayland compositor and feed it by hand
    weyes --backend manual &
    busctl --user call dev.weyes.Eyes /dev/weyes/Eyes dev.weyes.Eyes \
        Update s "900 150 100 100 304 144 0 0 1920 1080"
    #                cursor  window-origin+size  screen-rect
