//! The Vibe app icon, drawn in code so there is no image asset to load.
//!
//! It matches the SVG logo in `icons.rs`: a rounded square with a violet-to-pink
//! diagonal gradient, and a white down-arrow landing on a wave. Shapes are
//! defined on the logo's 64×64 grid and scaled to the requested size; each
//! pixel is anti-aliased by testing a 4×4 grid of sample points inside it.

/// Gradient ends: top-left and bottom-right.
const VIOLET: [f32; 3] = [139.0, 92.0, 246.0];
const PINK: [f32; 3] = [236.0, 72.0, 153.0];
/// Corner radius of the tile, on the 64 grid.
const RADIUS: f32 = 16.0;
/// Half the stroke widths, on the 64 grid.
const ARROW_HALF: f32 = 2.75;
const WAVE_HALF: f32 = 2.5;
/// Sample points per pixel along each axis (4x4 supersampling).
const SAMPLES: u32 = 4;

/// The Vibe icon at `size`×`size` pixels, RGBA8, transparent outside the tile.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let strokes = strokes();
    // Keep strokes at least ~1.5 px wide at tiny sizes.
    let min_half = 0.75 * 64.0 / size.max(1) as f32;
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    let total = SAMPLES * SAMPLES;
    let scale = 64.0 / size.max(1) as f32;
    for y in 0..size {
        for x in 0..size {
            let (mut inside, mut sum) = (0u32, [0f32; 3]);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let gx = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) * scale;
                    let gy = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) * scale;
                    if !in_tile(gx, gy) {
                        continue;
                    }
                    inside += 1;
                    let white = strokes.iter().any(|s| s.contains(gx, gy, min_half));
                    let colour = if white { [255.0; 3] } else { gradient(gx, gy) };
                    for (acc, c) in sum.iter_mut().zip(colour) {
                        *acc += c;
                    }
                }
            }
            if inside == 0 {
                px.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let alpha = (inside * 255 + total / 2) / total;
            for acc in sum {
                px.push((acc / inside as f32).round().clamp(0.0, 255.0) as u8);
            }
            px.push(alpha as u8);
        }
    }
    px
}

/// Inside the rounded 64×64 tile.
fn in_tile(x: f32, y: f32) -> bool {
    if !(0.0..=64.0).contains(&x) || !(0.0..=64.0).contains(&y) {
        return false;
    }
    let cx = x.clamp(RADIUS, 64.0 - RADIUS);
    let cy = y.clamp(RADIUS, 64.0 - RADIUS);
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= RADIUS * RADIUS
}

/// The diagonal brand gradient at a point of the 64 grid.
fn gradient(x: f32, y: f32) -> [f32; 3] {
    let t = ((x + y) / 128.0).clamp(0.0, 1.0);
    [0, 1, 2].map(|i| VIOLET[i] + (PINK[i] - VIOLET[i]) * t)
}

/// A line segment with round caps.
struct Stroke {
    a: (f32, f32),
    b: (f32, f32),
    half: f32,
}

impl Stroke {
    fn contains(&self, x: f32, y: f32, min_half: f32) -> bool {
        let (ax, ay) = self.a;
        let (bx, by) = self.b;
        let (dx, dy) = (bx - ax, by - ay);
        let len2 = dx * dx + dy * dy;
        let t = if len2 == 0.0 {
            0.0
        } else {
            (((x - ax) * dx + (y - ay) * dy) / len2).clamp(0.0, 1.0)
        };
        let (px, py) = (ax + t * dx - x, ay + t * dy - y);
        let half = self.half.max(min_half);
        px * px + py * py <= half * half
    }
}

/// The arrow and the wave as round-capped segments (same paths as the SVG logo).
fn strokes() -> Vec<Stroke> {
    let mut out = vec![
        Stroke { a: (32.0, 13.0), b: (32.0, 35.0), half: ARROW_HALF },
        Stroke { a: (22.5, 26.5), b: (32.0, 36.0), half: ARROW_HALF },
        Stroke { a: (41.5, 26.5), b: (32.0, 36.0), half: ARROW_HALF },
    ];
    // "M15 47 c4.3-4 8.7-4 13 0 s8.7 4 13 0 6.5-3 8 0", as absolute cubics.
    let curves = [
        [(15.0, 47.0), (19.3, 43.0), (23.7, 43.0), (28.0, 47.0)],
        [(28.0, 47.0), (32.3, 51.0), (36.7, 51.0), (41.0, 47.0)],
        [(41.0, 47.0), (45.3, 43.0), (47.5, 44.0), (49.0, 47.0)],
    ];
    const STEPS: usize = 12;
    for [p0, p1, p2, p3] in curves {
        let point = |t: f32| {
            let u = 1.0 - t;
            let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            (a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0, a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1)
        };
        for i in 0..STEPS {
            let (t0, t1) = (i as f32 / STEPS as f32, (i + 1) as f32 / STEPS as f32);
            out.push(Stroke { a: point(t0), b: point(t1), half: WAVE_HALF });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(px: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    #[test]
    fn length_is_size_squared_times_four() {
        for size in [1, 16, 24, 32, 48, 64, 128, 256] {
            assert_eq!(icon_rgba(size).len(), (size * size * 4) as usize, "size {size}");
        }
    }

    #[test]
    fn corners_are_transparent() {
        for size in [16, 32, 256] {
            let last = size - 1;
            for (x, y) in [(0, 0), (last, 0), (0, last), (last, last)] {
                assert_eq!(pixel(&icon_rgba(size), size, x, y)[3], 0, "size {size} ({x},{y})");
            }
        }
    }

    #[test]
    fn arrow_shaft_is_white() {
        for size in [32, 256] {
            let px = icon_rgba(size);
            // On the shaft, a bit above the centre.
            let (x, y) = (size / 2, size * 3 / 8);
            assert_eq!(pixel(&px, size, x, y), [255, 255, 255, 255], "size {size}");
        }
    }

    #[test]
    fn tile_runs_from_violet_to_pink() {
        let size = 256;
        let px = icon_rgba(size);
        let top_left = pixel(&px, size, 40, 40);
        let bottom_right = pixel(&px, size, 215, 215);
        assert_eq!(top_left[3], 255);
        assert_eq!(bottom_right[3], 255);
        // Violet has more blue than red; pink more red than blue.
        assert!(top_left[2] > top_left[0], "{top_left:?}");
        assert!(bottom_right[0] > bottom_right[2], "{bottom_right:?}");
    }

    #[test]
    fn edge_is_anti_aliased_and_not_white() {
        let size = 256;
        let px = icon_rgba(size);
        let partial: Vec<&[u8; 4]> =
            px.as_chunks::<4>().0.iter().filter(|p| p[3] > 0 && p[3] < 255).collect();
        assert!(!partial.is_empty(), "some edge pixels are partly transparent");
        assert!(partial.iter().all(|p| p[..3] != [255, 255, 255]), "strokes stay off the rim");
    }

    #[test]
    fn small_sizes_still_show_the_arrow() {
        let size = 16;
        let px = icon_rgba(size);
        let bright =
            px.as_chunks::<4>().0.iter().filter(|p| p[0] > 230 && p[1] > 230 && p[2] > 230).count();
        assert!(bright >= 6, "only {bright} white pixels at 16 px");
    }
}
