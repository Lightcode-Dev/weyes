//! The pure, dependency-free heart of weyes: a straight port of the logic in
//! xeyes' `Eyes.c` / `transform.c`. Nothing in here knows about Wayland, D-Bus
//! or drawing, which keeps it easy to unit-test.

// --- Geometry constants (Eyes.c) -------------------------------------------
// All of these live in "eye space", where one eye is 2.0 units wide.

/// Padding between eyes.
pub const EYE_OFFSET: f64 = 0.1;
/// Thickness of the eye rim.
pub const EYE_THICK: f64 = 0.175;
/// Diameter of the pupil.
pub const BALL_DIAM: f64 = 0.3;
/// Gap kept between the pupil and the rim.
pub const BALL_PAD: f64 = 0.175;
/// Diameter of the white of the eye (inside the rim).
pub const EYE_DIAM: f64 = 2.0 - (EYE_THICK + EYE_OFFSET) * 2.0;
/// How far from the eye's center the pupil is allowed to travel.
pub const BALL_DIST: f64 = (EYE_DIAM - BALL_DIAM) / 2.0 - BALL_PAD;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

// --- Eye layouts -----------------------------------------------------------

const LAYOUT_STANDARD: &[Point] = &[Point { x: 0.0, y: 0.0 }, Point { x: 2.0, y: 0.0 }];

const LAYOUT_BIBLICAL: &[Point] = &[
    Point { x: 0.75, y: 0.0 },
    Point { x: 2.25, y: 0.0 },
    Point { x: 3.75, y: 0.0 },
    Point { x: 0.0, y: 1.4 },
    Point { x: 1.5, y: 1.4 },
    Point { x: 3.0, y: 1.4 },
    Point { x: 4.5, y: 1.4 },
    Point { x: 0.75, y: 2.8 },
    Point { x: 2.25, y: 2.8 },
    Point { x: 3.75, y: 2.8 },
];

/// Where the eyes are, and the bounding box (in eye space) that the window
/// should map onto.
#[derive(Clone, Debug)]
pub struct EyeConfig {
    pub eyes: &'static [Point],
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

impl EyeConfig {
    pub fn new(biblically_accurate: bool) -> Self {
        let eyes = if biblically_accurate { LAYOUT_BIBLICAL } else { LAYOUT_STANDARD };

        let mut c = EyeConfig {
            eyes,
            min_x: eyes[0].x,
            max_x: eyes[0].x,
            min_y: eyes[0].y,
            max_y: eyes[0].y,
        };
        for e in eyes {
            c.min_x = c.min_x.min(e.x);
            c.max_x = c.max_x.max(e.x);
            c.min_y = c.min_y.min(e.y);
            c.max_y = c.max_y.max(e.y);
        }
        // Add the half-size of an eye (1.0) minus the padding to every edge.
        let pad = 1.0 - EYE_OFFSET;
        c.min_x -= pad;
        c.max_x += pad;
        c.min_y -= pad;
        c.max_y += pad;
        c
    }

    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }
}

// --- Coordinate transform (transform.c) -----------------------------------

/// A scale + offset on each axis. `my` is negative for our use: eye space has
/// +y up, window space has +y down. This is the same flip xeyes does with
/// `SetTransform(0, width, height, 0, ...)`.
#[derive(Clone, Copy, Debug)]
pub struct Transform {
    pub mx: f64,
    pub bx: f64,
    pub my: f64,
    pub by: f64,
}

impl Transform {
    /// Map eye space onto a `width` x `height` window (y flipped).
    pub fn for_window(cfg: &EyeConfig, width: f64, height: f64) -> Self {
        Self::new(0.0, width, height, 0.0, cfg.min_x, cfg.max_x, cfg.min_y, cfg.max_y)
    }

    /// Port of `SetTransform`: window range [xx1, xx2] x [xy1, xy2] <-> eye
    /// range [tx1, tx2] x [ty1, ty2].
    #[allow(clippy::too_many_arguments)]
    pub fn new(xx1: f64, xx2: f64, xy1: f64, xy2: f64, tx1: f64, tx2: f64, ty1: f64, ty2: f64) -> Self {
        let mx = (xx2 - xx1) / (tx2 - tx1);
        let bx = xx1 - mx * tx1;
        let my = (xy2 - xy1) / (ty2 - ty1);
        let by = xy1 - my * ty1;
        Transform { mx, bx, my, by }
    }

    /// Eye space -> window space (`Xx`/`Xy`, minus the integer rounding).
    pub fn to_window(&self, p: Point) -> Point {
        Point { x: self.mx * p.x + self.bx, y: self.my * p.y + self.by }
    }

    /// Window space -> eye space (`Tx`/`Ty`).
    pub fn to_eye(&self, p: Point) -> Point {
        Point { x: (p.x - self.bx) / self.mx, y: (p.y - self.by) / self.my }
    }

    /// Window-space size -> eye-space size (`Twidth`/`Theight`). Note this can
    /// be negative in y; `compute_pupil`'s distance mode depends on that.
    pub fn size_to_eye(&self, w: f64, h: f64) -> (f64, f64) {
        (w / self.mx, h / self.my)
    }
}

// --- The actual eyeball math (Eyes.c: computePupil) -----------------------

fn angle_between(a: f64, a0: f64, a1: f64) -> bool {
    if a0 <= a1 {
        a0 <= a && a <= a1
    } else {
        a0 <= a || a <= a1
    }
}

/// Where the pupil of `eye` goes when the mouse is at `mouse` (both in eye
/// space).
///
/// With `screen == None` the pupil simply points toward the mouse and stops at
/// `BALL_DIST`. With `Some(screen)` ("distance mode") the travel is scaled by
/// how far the mouse is toward the edge of the screen in that direction, so the
/// pupils drift smoothly instead of snapping to the rim as soon as the mouse
/// leaves the eye.
pub fn compute_pupil(eye: Point, mouse: Point, screen: Option<Rect>) -> Point {
    let dx = mouse.x - eye.x;
    let dy = mouse.y - eye.y;

    // Mouse dead-center: leave the pupil in the middle.
    if dx == 0.0 && dy == 0.0 {
        return eye;
    }

    let angle = dy.atan2(dx);
    let (sina, cosa) = angle.sin_cos();
    let mut dist = BALL_DIST;

    if let Some(s) = screen {
        // Corners of the screen, relative to this eye's center.
        let x0 = s.x - eye.x;
        let y0 = s.y - eye.y;
        let x1 = x0 + s.width;
        let y1 = y0 + s.height;
        let a = [y0.atan2(x0), y1.atan2(x0), y1.atan2(x1), y0.atan2(x1)];

        // Which edge of the screen is the mouse heading toward? Scale by the
        // fraction of the way to that edge.
        if angle_between(angle, a[0], a[1]) {
            dist *= dx / x0; // left
        } else if angle_between(angle, a[1], a[2]) {
            dist *= dy / y1; // bottom
        } else if angle_between(angle, a[2], a[3]) {
            dist *= dx / x1; // right
        } else if angle_between(angle, a[3], a[0]) {
            dist *= dy / y0; // top
        }

        // Deviation from xeyes: if an eye sits exactly on a screen edge, or
        // the window is partly off-screen, the ratios above can come out NaN,
        // infinite or negative. Clamp to something sane rather than letting a
        // NaN poison the drawing.
        dist = if dist.is_finite() { dist.clamp(0.0, BALL_DIST) } else { BALL_DIST };
    }

    if dist > dx.hypot(dy) {
        // Mouse is closer than the pupil's max travel: pupil sits on the mouse.
        Point { x: eye.x + dx, y: eye.y + dy }
    } else {
        Point { x: eye.x + dist * cosa, y: eye.y + dist * sina }
    }
}

/// Pupils for every eye in `cfg`.
pub fn compute_pupils(cfg: &EyeConfig, mouse: Point, screen: Option<Rect>) -> Vec<Point> {
    cfg.eyes.iter().map(|&e| compute_pupil(e, mouse, screen)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < EPS && (a.y - b.y).abs() < EPS
    }

    #[test]
    fn constants_match_xeyes() {
        assert!((EYE_DIAM - 1.45).abs() < EPS);
        assert!((BALL_DIST - 0.4).abs() < EPS); // (1.45 - 0.3) / 2 - 0.175
    }

    #[test]
    fn standard_bounds() {
        let c = EyeConfig::new(false);
        assert!((c.min_x - -0.9).abs() < EPS);
        assert!((c.max_x - 2.9).abs() < EPS);
        assert!((c.min_y - -0.9).abs() < EPS);
        assert!((c.max_y - 0.9).abs() < EPS);
    }

    #[test]
    fn transform_round_trips_and_flips_y() {
        let c = EyeConfig::new(false);
        let t = Transform::for_window(&c, 380.0, 180.0);
        // Left-bottom of eye space is window bottom-left (y flipped).
        let p = t.to_window(Point { x: c.min_x, y: c.min_y });
        assert!(close(p, Point { x: 0.0, y: 180.0 }));
        let p = t.to_window(Point { x: c.max_x, y: c.max_y });
        assert!(close(p, Point { x: 380.0, y: 0.0 }));
        let q = Point { x: 1.234, y: -0.4 };
        assert!(close(t.to_eye(t.to_window(q)), q));
    }

    #[test]
    fn mouse_far_right_pushes_pupil_to_the_rim() {
        let eye = Point { x: 0.0, y: 0.0 };
        let p = compute_pupil(eye, Point { x: 100.0, y: 0.0 }, None);
        assert!(close(p, Point { x: BALL_DIST, y: 0.0 }));
    }

    #[test]
    fn mouse_inside_reach_puts_pupil_on_the_mouse() {
        let eye = Point { x: 2.0, y: 0.0 };
        let m = Point { x: 2.1, y: 0.05 };
        assert!(close(compute_pupil(eye, m, None), m));
    }

    #[test]
    fn mouse_dead_center_keeps_pupil_centered() {
        let eye = Point { x: 2.0, y: 0.0 };
        assert!(close(compute_pupil(eye, eye, None), eye));
    }

    #[test]
    fn pupil_never_leaves_ball_dist_even_in_distance_mode() {
        let c = EyeConfig::new(true);
        let t = Transform::for_window(&c, 300.0, 200.0);
        let (sw, sh) = t.size_to_eye(1920.0, 1080.0);
        let origin = t.to_eye(Point { x: -500.0, y: -300.0 });
        let screen = Rect { x: origin.x, y: origin.y, width: sw, height: sh };

        for ix in -20..=40 {
            for iy in -20..=40 {
                let m = Point { x: ix as f64 * 0.7, y: iy as f64 * 0.7 };
                for &eye in c.eyes {
                    for scr in [None, Some(screen)] {
                        let p = compute_pupil(eye, m, scr);
                        assert!(p.x.is_finite() && p.y.is_finite(), "NaN for mouse {m:?}");
                        let d = (p.x - eye.x).hypot(p.y - eye.y);
                        assert!(d <= BALL_DIST + EPS, "pupil escaped: {d}");
                    }
                }
            }
        }
    }

    #[test]
    fn distance_mode_moves_less_when_mouse_is_nearer() {
        // A 1000x1000 "screen" in eye space with the eye in its middle.
        let eye = Point { x: 0.0, y: 0.0 };
        let screen = Rect { x: -500.0, y: 500.0, width: 1000.0, height: -1000.0 };
        let near = compute_pupil(eye, Point { x: 50.0, y: 0.0 }, Some(screen));
        let far = compute_pupil(eye, Point { x: 400.0, y: 0.0 }, Some(screen));
        assert!(near.x < far.x);
        assert!(far.x <= BALL_DIST + EPS);
    }
}
