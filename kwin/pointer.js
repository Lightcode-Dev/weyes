// weyes: KWin-side pointer feed.
//
// Wayland clients can't see the pointer outside their own surface, and can't
// see where their own window is on screen. KWin knows both, so this script
// (which runs *inside* the compositor) pushes them to weyes over D-Bus:
//
//   Update("cursorX cursorY winX winY winW winH screenX screenY screenW screenH")
//
// Everything is in KWin's global logical coordinates. The payload is a single
// string on purpose: JS numbers can arrive over D-Bus as either int or double
// depending on the value, and a string sidesteps that type ambiguity.
//
// Keep this script cheap. It runs on the compositor's main thread.

const SERVICE = "dev.weyes.Eyes";
const PATH = "/dev/weyes/Eyes";
const IFACE = "dev.weyes.Eyes";
const APP_ID = "weyes"; // must match the xdg_toplevel app_id weyes sets

let ours = null;       // our window, once found
let lastScan = 0;      // ms timestamp of the last fallback window scan
let lastSent = "";     // last payload, to drop exact duplicates
let screenRect = { x: 0, y: 0, width: 0, height: 0 };

function matches(w) {
    if (!w) return false;
    const ids = [w.resourceClass, w.resourceName, w.desktopFileName];
    for (const id of ids) {
        if (id && String(id).toLowerCase() === APP_ID) return true;
    }
    return false;
}

// Bounding box of all outputs (the "virtual screen").
function refreshScreens() {
    let x0 = 0, y0 = 0, x1 = 0, y1 = 0, first = true;
    for (const o of workspace.screens) {
        const g = o.geometry;
        if (first) {
            x0 = g.x; y0 = g.y; x1 = g.x + g.width; y1 = g.y + g.height;
            first = false;
        } else {
            x0 = Math.min(x0, g.x);
            y0 = Math.min(y0, g.y);
            x1 = Math.max(x1, g.x + g.width);
            y1 = Math.max(y1, g.y + g.height);
        }
    }
    screenRect = { x: x0, y: y0, width: x1 - x0, height: y1 - y0 };
}

function push() {
    const c = workspace.cursorPos;
    const g = ours ? ours.frameGeometry : null;
    const msg = [
        c.x, c.y,
        g ? g.x : 0, g ? g.y : 0, g ? g.width : 0, g ? g.height : 0,
        screenRect.x, screenRect.y, screenRect.width, screenRect.height,
    ].map(Math.round).join(" ");
    if (msg === lastSent) return;
    lastSent = msg;
    callDBus(SERVICE, PATH, IFACE, "Update", msg);
}

function adopt(w) {
    if (ours === w) return;
    ours = w;
    // Re-send whenever the user drags or resizes the eyes.
    w.frameGeometryChanged.connect(push);
    push();
}

function scan() {
    const list = (typeof workspace.windowList === "function")
        ? workspace.windowList()
        : workspace.stackingOrder;
    for (const w of list) {
        if (matches(w)) { adopt(w); return; }
    }
}

workspace.cursorPosChanged.connect(function () {
    // If we haven't found our window yet, retry occasionally.
    if (!ours) {
        const now = Date.now();
        if (now - lastScan > 1000) { lastScan = now; scan(); }
    }
    push();
});

workspace.windowAdded.connect(function (w) {
    if (matches(w)) adopt(w);
});

workspace.windowRemoved.connect(function (w) {
    if (w === ours) { ours = null; lastSent = ""; push(); }
});

workspace.screensChanged.connect(function () { refreshScreens(); push(); });

refreshScreens();
scan();
push();
