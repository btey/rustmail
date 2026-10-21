//! macOS traffic lights for screenshots.
//!
//! AppKit draws the native window controls, so neither the offscreen
//! renderer nor slint-viewer can. Screenshots paint them in the standard
//! macOS colours where the application moves the native buttons. Shared by
//! the frameless screenshot test and `examples/paint-screenshot-traffic-lights.rs`,
//! so this file depends on nothing but `image`.

use image::RgbImage;

/// Where the application places the native buttons, in logical pixels.
pub(crate) struct Metrics {
    /// Left edge of the first light.
    pub(crate) x: f32,
    /// Distance between the left edges of neighbouring lights.
    pub(crate) spacing: f32,
    pub(crate) size: f32,
    pub(crate) center_y: f32,
}

/// The standard macOS window-control colours: each light's fill and the
/// slightly darker rim AppKit draws around it.
struct TrafficLight {
    fill: [u8; 3],
    rim: [u8; 3],
}

const TRAFFIC_LIGHTS: [TrafficLight; 3] = [
    TrafficLight {
        fill: [0xff, 0x5f, 0x57],
        rim: [0xe0, 0x44, 0x3e],
    },
    TrafficLight {
        fill: [0xfe, 0xbc, 0x2e],
        rim: [0xde, 0xa1, 0x23],
    },
    TrafficLight {
        fill: [0x28, 0xc8, 0x40],
        rim: [0x1a, 0xab, 0x29],
    },
];

/// Paint the three lights onto an image rendered at `scale`.
pub(crate) fn paint(image: &mut RgbImage, scale: f32, metrics: &Metrics) {
    for (index, light) in TRAFFIC_LIGHTS.iter().enumerate() {
        let center_x = metrics.x + index as f32 * metrics.spacing + metrics.size / 2.0;
        paint_light(
            image,
            (center_x * scale, metrics.center_y * scale),
            metrics.size * scale,
            0.5 * scale,
            light,
        );
    }
}

/// Paint one antialiased light of `diameter` (physical pixels) with a
/// hairline rim.
fn paint_light(
    image: &mut RgbImage,
    center: (f32, f32),
    diameter: f32,
    rim_width: f32,
    light: &TrafficLight,
) {
    let radius = diameter / 2.0;
    let (cx, cy) = center;
    let x0 = (cx - radius - 1.0).floor().max(0.0) as u32;
    let y0 = (cy - radius - 1.0).floor().max(0.0) as u32;
    let x1 = ((cx + radius + 1.0).ceil() as u32).min(image.width());
    let y1 = ((cy + radius + 1.0).ceil() as u32).min(image.height());
    const SAMPLES: usize = 4;
    for y in y0..y1 {
        for x in x0..x1 {
            let mut inside = 0.0;
            let mut rim = 0.0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32 - cx;
                    let py = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32 - cy;
                    let distance = (px * px + py * py).sqrt();
                    if distance <= radius {
                        inside += 1.0;
                        if distance > radius - rim_width {
                            rim += 1.0;
                        }
                    }
                }
            }
            if inside == 0.0 {
                continue;
            }
            let coverage = inside / (SAMPLES * SAMPLES) as f32;
            let rim = rim / inside;
            let pixel = image.get_pixel_mut(x, y);
            for channel in 0..3 {
                let fill = f32::from(light.fill[channel]);
                let color = fill + (f32::from(light.rim[channel]) - fill) * rim;
                let base = f32::from(pixel.0[channel]);
                pixel.0[channel] = (base * (1.0 - coverage) + color * coverage).round() as u8;
            }
        }
    }
}
