//! Drawing. Pure software rasterization with tiny-skia into an RGBA pixmap,
//! then a copy into the Wayland shared-memory buffer.
//!
//! This replaces xeyes' X11 pieces: XFillArc/XRender for drawing and the SHAPE
//! extension for the non-rectangular window. On Wayland the "shape" is simply
//! the alpha channel: pixels outside the eyes stay fully transparent.

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect as SkRect, Transform as SkTransform};

use crate::core::{BALL_DIAM, EYE_DIAM, EYE_THICK, EyeConfig, Point, Transform};

/// xeyes' defaults: black rim and pupil, white eye.
#[derive(Clone, Copy, Debug)]
pub struct Colors {
    pub outline: Color,
    pub center: Color,
    pub pupil: Color,
}

impl Default for Colors {
    fn default() -> Self {
        Colors { outline: Color::BLACK, center: Color::WHITE, pupil: Color::BLACK }
    }
}

/// Fill a circle (an ellipse once the window isn't square) of eye-space
/// diameter `diam`, centered on eye-space `(cx, cy)`.
fn ellipse(pm: &mut Pixmap, t: &Transform, cx: f64, cy: f64, diam: f64, color: Color) {
    let h = diam / 2.0;
    let a = t.to_window(Point { x: cx - h, y: cy - h });
    let b = t.to_window(Point { x: cx + h, y: cy + h });
    // y is flipped by the transform, so normalise the corners.
    let (l, r) = (a.x.min(b.x) as f32, a.x.max(b.x) as f32);
    let (top, bot) = (a.y.min(b.y) as f32, a.y.max(b.y) as f32);

    let Some(rect) = SkRect::from_ltrb(l, top, r, bot) else { return };
    let Some(path) = PathBuilder::from_oval(rect) else { return };

    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pm.fill_path(&path, &paint, FillRule::Winding, SkTransform::identity(), None);
}

/// Render one frame at `width` x `height` *buffer* pixels.
///
/// Order matches xeyes' `repaint_window`: every eye's rim then its white (so in
/// the overlapping "biblical" layout later eyes sit on top of earlier ones),
/// then every pupil.
pub fn render(cfg: &EyeConfig, pupils: &[Point], width: u32, height: u32, colors: &Colors) -> Option<Pixmap> {
    let mut pm = Pixmap::new(width, height)?;
    pm.fill(Color::TRANSPARENT);
    let t = Transform::for_window(cfg, width as f64, height as f64);

    for e in cfg.eyes {
        ellipse(&mut pm, &t, e.x, e.y, EYE_DIAM + 2.0 * EYE_THICK, colors.outline);
        ellipse(&mut pm, &t, e.x, e.y, EYE_DIAM, colors.center);
    }
    for p in pupils {
        ellipse(&mut pm, &t, p.x, p.y, BALL_DIAM, colors.pupil);
    }
    Some(pm)
}

/// Pupil positions rounded to whole buffer pixels. Comparing these between
/// frames tells us whether a redraw would actually change anything, which is
/// the same trick xeyes plays with `XPointEqual`.
pub fn pupil_pixels(cfg: &EyeConfig, pupils: &[Point], width: u32, height: u32) -> Vec<(i32, i32)> {
    let t = Transform::for_window(cfg, width as f64, height as f64);
    pupils
        .iter()
        .map(|&p| {
            let w = t.to_window(p);
            (w.x.round() as i32, w.y.round() as i32)
        })
        .collect()
}

/// Copy a tiny-skia pixmap into a `wl_shm` `Argb8888` buffer.
///
/// tiny-skia stores premultiplied RGBA bytes. `Argb8888` is a little-endian
/// 32-bit word, so in memory the bytes are B, G, R, A, also premultiplied (which
/// is what Wayland compositors expect). So it's just a red/blue swap.
pub fn copy_to_argb8888(pm: &Pixmap, dst: &mut [u8]) {
    for (s, d) in pm.data().chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = s[3];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argb8888_swaps_red_and_blue_only() {
        // tiny-skia byte order is R,G,B,A; wl_shm Argb8888 (little-endian) is B,G,R,A.
        let mut pm = Pixmap::new(1, 1).unwrap();
        pm.data_mut().copy_from_slice(&[10, 20, 30, 40]);
        let mut out = [0u8; 4];
        copy_to_argb8888(&pm, &mut out);
        assert_eq!(out, [30, 20, 10, 40]);
    }

    #[test]
    fn outside_the_eyes_is_transparent_and_the_pupil_is_opaque() {
        let cfg = EyeConfig::new(false);
        let (w, h) = (304, 144);
        let pm = render(&cfg, &cfg.eyes.to_vec(), w, h, &Colors::default()).unwrap();
        let px = |x: u32, y: u32| pm.pixel(x, y).unwrap();

        // Top-left corner is outside both eyes: the "shape" is just alpha.
        assert_eq!(px(1, 1).alpha(), 0);
        // Dead centre of the left eye is its pupil: opaque black.
        let c = Transform::for_window(&cfg, w as f64, h as f64).to_window(cfg.eyes[0]);
        let p = px(c.x as u32, c.y as u32);
        assert_eq!((p.red(), p.green(), p.blue(), p.alpha()), (0, 0, 0, 255));
    }

    #[test]
    fn unchanged_pupils_mean_no_redraw() {
        let cfg = EyeConfig::new(false);
        let a = pupil_pixels(&cfg, &cfg.eyes.to_vec(), 304, 144);
        // A sub-pixel nudge shouldn't register as a change.
        let nudged: Vec<Point> = cfg.eyes.iter().map(|p| Point { x: p.x + 0.0005, y: p.y }).collect();
        assert_eq!(a, pupil_pixels(&cfg, &nudged, 304, 144));
    }
}
