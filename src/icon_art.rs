const TOP_COLOR: [u8; 3] = [0x3b, 0x82, 0xf6];

const BOTTOM_COLOR: [u8; 3] = [0x7c, 0x3a, 0xed];

const GLYPH: [u8; 3] = [0xff, 0xff, 0xff];

const SS: u32 = 3;

pub fn draw(size: u32) -> Vec<u8> {
    assert!(size > 0, "icon size must be positive");
    let n = size as usize;

    let mut acc = vec![0.0f32; n * n * 4];

    for py in 0..size {
        for px in 0..size {
            let mut tile_r = 0.0;
            let mut tile_g = 0.0;
            let mut tile_b = 0.0;
            let mut tile_a = 0.0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    let (r, g, b, a) = sample(x, y, size);
                    tile_r += r * a;
                    tile_g += g * a;
                    tile_b += b * a;
                    tile_a += a;
                }
            }
            let samples = (SS * SS) as f32;
            let idx = (py as usize * n + px as usize) * 4;
            let alpha = tile_a / samples;
            if alpha <= 0.0 {
                continue;
            }

            acc[idx] = tile_r / tile_a;
            acc[idx + 1] = tile_g / tile_a;
            acc[idx + 2] = tile_b / tile_a;
            acc[idx + 3] = alpha * 255.0;
        }
    }

    let mut out = Vec::with_capacity(n * n * 4);
    for value in acc {
        out.push(value.round().clamp(0.0, 255.0) as u8);
    }
    out
}

fn sample(x: f32, y: f32, size: u32) -> (f32, f32, f32, f32) {
    if !in_rounded_tile(x, y) {
        return (0.0, 0.0, 0.0, 0.0);
    }
    if in_glyph(x, y, size) {
        return (GLYPH[0] as f32, GLYPH[1] as f32, GLYPH[2] as f32, 1.0);
    }

    let t = ((y * 0.85) + (x * 0.15)).clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| a as f32 + (b as f32 - a as f32) * t;
    (
        lerp(TOP_COLOR[0], BOTTOM_COLOR[0]),
        lerp(TOP_COLOR[1], BOTTOM_COLOR[1]),
        lerp(TOP_COLOR[2], BOTTOM_COLOR[2]),
        1.0,
    )
}

fn in_rounded_tile(x: f32, y: f32) -> bool {
    const R: f32 = 0.225;
    let cx = x.clamp(R, 1.0 - R);
    let cy = y.clamp(R, 1.0 - R);
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= R * R
}

fn in_glyph(x: f32, y: f32, size: u32) -> bool {
    let box_rect = (0.20..=0.345).contains(&x) && (0.415..=0.585).contains(&y);

    let cone = in_trapezoid(x, y, 0.345, 0.50, 0.085, 0.215);

    let arcs: &[Arc] = if size <= 24 { &ARCS_SMALL } else { &ARCS_BIG };
    box_rect || cone || arcs.iter().any(|arc| arc.contains(x, y))
}

struct Arc {
    cx: f32,
    cy: f32,
    r: f32,
    half: f32,
    span: f32,
}

impl Arc {
    fn contains(&self, x: f32, y: f32) -> bool {
        let dx = x - self.cx;
        let dy = y - self.cy;
        let dist = (dx * dx + dy * dy).sqrt();
        if (dist - self.r).abs() > self.half {
            return false;
        }
        let angle = dy.atan2(dx);
        angle >= -self.span && angle <= self.span
    }
}

const ARCS_BIG: [Arc; 2] = [
    Arc {
        cx: 0.52,
        cy: 0.5,
        r: 0.16,
        half: 0.042,
        span: 0.78,
    },
    Arc {
        cx: 0.52,
        cy: 0.5,
        r: 0.27,
        half: 0.042,
        span: 0.68,
    },
];
const ARCS_SMALL: [Arc; 1] = [Arc {
    cx: 0.53,
    cy: 0.5,
    r: 0.25,
    half: 0.06,
    span: 0.72,
}];

fn in_trapezoid(
    x: f32,
    y: f32,
    x_left: f32,
    x_right: f32,
    half_left: f32,
    half_right: f32,
) -> bool {
    if x < x_left || x > x_right {
        return false;
    }
    let t = (x - x_left) / (x_right - x_left);
    let half = half_left + (half_right - half_left) * t;
    (y - 0.5).abs() <= half
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(px: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * size + x) * 4) as usize;
        [px[idx], px[idx + 1], px[idx + 2], px[idx + 3]]
    }

    #[test]
    fn produces_rgba_for_size() {
        assert_eq!(draw(16).len(), 16 * 16 * 4);
        assert_eq!(draw(64).len(), 64 * 64 * 4);
        assert_eq!(draw(1).len(), 4);
    }

    #[test]
    fn corners_are_transparent() {
        let size = 64;
        let px = draw(size);
        for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
            assert_eq!(
                pixel(&px, size, x, y)[3],
                0,
                "corner ({x},{y}) must be see-through"
            );
        }
    }

    #[test]
    fn tile_interior_is_opaque_and_hued() {
        let size = 64;
        let px = draw(size);

        let [r, g, b, a] = pixel(&px, size, 3, 32);
        assert!(a > 200, "tile edge should be opaque, got alpha {a}");

        assert!(b > r + 40, "expected blue tint, got rgb({r},{g},{b})");
        assert!(!(r > 240 && g > 240), "should not be the white glyph");
    }

    #[test]
    fn glyph_is_white_and_present() {
        let size = 64;
        let px = draw(size);

        let [r, g, b, a] = pixel(&px, size, (0.27 * size as f32) as u32, 32);
        assert!(a > 200, "glyph should be opaque");
        assert!(
            r > 230 && g > 230 && b > 230,
            "glyph should be near-white, got ({r},{g},{b})"
        );
    }

    #[test]
    fn gradient_runs_from_blue_to_violet() {
        let top = sample(0.5, 0.18, 64);
        let bottom = sample(0.5, 0.82, 64);
        assert!(top.2 > top.0, "top should lean blue (b > r)");
        assert!(bottom.0 > top.0, "bottom should lean violet (r grows)");
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(draw(32), draw(32));
    }

    #[test]
    fn every_size_is_fully_drawn_somewhere() {
        let px = draw(16);
        let opaque = px.chunks(4).filter(|p| p[3] > 200).count();
        assert!(opaque > 16 * 16 / 3, "too few opaque pixels: {opaque}");
    }
}
