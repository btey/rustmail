//! Paint macOS traffic lights onto desktop screenshots that slint-viewer
//! rendered with the integrated macOS window frame.

#[path = "../src/screenshot_traffic_lights.rs"]
mod screenshot_traffic_lights;

use screenshot_traffic_lights::Metrics;
use std::env;

/// generate-screenshots.sh renders at SLINT_SCALE_FACTOR=2.
const SCALE: f32 = 2.0;

/// `WindowChrome`'s macOS 26 defaults, centred on the 44px desktop top bar.
const METRICS: Metrics = Metrics {
    x: 20.0,
    spacing: 23.0,
    size: 14.0,
    center_y: 22.0,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<_> = env::args_os().skip(1).collect();
    if paths.is_empty() {
        return Err("usage: paint-screenshot-traffic-lights SCREENSHOT...".into());
    }
    for path in paths {
        let mut image = image::open(&path)?.into_rgb8();
        screenshot_traffic_lights::paint(&mut image, SCALE, &METRICS);
        image.save(&path)?;
    }
    Ok(())
}
