use super::*;
use blitz_dom::util::{Color, ToColorColor};

fn dark() -> EmailAppearance {
    EmailAppearance {
        original: false,
        dark: true,
        background: Color::from_rgb8(24, 25, 26),
        foreground: Color::from_rgb8(243, 243, 242),
        link: Color::from_rgb8(92, 163, 255),
    }
}

fn prepared(html: &str, appearance: EmailAppearance) -> PreparedEmail {
    prepare_email_html_at(html, None, 520, 900, 1.0, appearance).unwrap()
}

fn colors(email: &PreparedEmail, selector: &str) -> (Color, Color) {
    let id = email.document.query_selector(selector).unwrap().unwrap();
    let style = email
        .document
        .get_node(id)
        .unwrap()
        .primary_styles()
        .unwrap();
    let foreground = style.clone_color();
    (
        foreground.as_color_color(),
        style
            .get_background()
            .background_color
            .resolve_to_absolute(&foreground)
            .as_color_color(),
    )
}

fn contrast(a: Color, b: Color) -> f32 {
    let (a, b) = (
        a.split().0.relative_luminance(),
        b.split().0.relative_luminance(),
    );
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn assert_color(actual: Color, expected: Color) {
    for (actual, expected) in actual.components.into_iter().zip(expected.components) {
        assert!(
            (actual - expected).abs() < 0.00001,
            "color changed: {actual}, {expected}"
        );
    }
}

fn pixels(email: &mut PreparedEmail) -> slint::SharedPixelBuffer<slint::Rgba8Pixel> {
    render_prepared_cpu(email, 520, 900, 1.0).unwrap().tiles[0]
        .image
        .to_rgba8()
        .unwrap()
}

#[test]
fn dark_html_covers_legacy_tables_inline_important_buttons_and_gradients() {
    let mut email = prepared(
        r##"<body bgcolor="#ffffff" style="margin:0;color:#000 !important">
      <table><tr bgcolor="#f5f5f5"><td id="cell" bgcolor="#fff" style="color:#303348 !important">Readable prose</td></tr></table>
      <a id="cta" href="https://example.com" style="background:#4b5fff;color:#fff !important;padding:12px">Open</a>
      <div id="gradient" style="background:linear-gradient(90deg,#fff,#ffeeaa);color:#111">Gradient text</div>
      <p id="hidden" style="color:transparent">Hidden text</p>
      <p id="preheader" style="background:#fff;color:#fff">Hidden preview padding <span id="nested-preheader">including nested text</span></p>
      <p id="current" style="background:linear-gradient(currentColor,currentColor);color:white">Gradient</p>
    </body>"##,
        dark(),
    );
    let (fg, bg) = colors(&email, "#cell");
    assert!(bg.split().0.relative_luminance() < 0.05);
    assert!(
        contrast(fg, bg) >= 4.5,
        "text lost contrast: {fg:?}, {bg:?}"
    );
    let (fg, bg) = colors(&email, "#cta");
    assert!(contrast(fg, bg) >= 4.5);
    assert_color(bg, Color::from_rgb8(75, 95, 255));
    assert_eq!(colors(&email, "#hidden").0.components[3], 0.0);
    let (fg, bg) = colors(&email, "#preheader");
    assert_color(fg, bg);
    assert_color(colors(&email, "#nested-preheader").0, bg);
    let current = email.document.query_selector("#current").unwrap().unwrap();
    let current_css = {
        let styles = email
            .document
            .get_node(current)
            .unwrap()
            .primary_styles()
            .unwrap();
        style_traits::ToCss::to_css_string(&styles.get_background().background_image.0[0])
    };
    assert!(
        !current_css.contains("currentcolor"),
        "gradient must retain its resolved color: {current_css}"
    );
    let gradient = email.document.query_selector("#gradient").unwrap().unwrap();
    let css = {
        let styles = email
            .document
            .get_node(gradient)
            .unwrap()
            .primary_styles()
            .unwrap();
        style_traits::ToCss::to_css_string(&styles.get_background().background_image.0[0])
    };
    assert!(
        !css.contains("rgb(255, 255, 255)"),
        "gradient stayed white: {css}"
    );
    let frame = pixels(&mut email);
    assert!(
        frame
            .as_slice()
            .iter()
            .any(|p| p.r > 200 && p.g > 200 && p.b > 200),
        "light text should be painted"
    );
    assert!(
        frame
            .as_slice()
            .iter()
            .filter(|p| p.r < 70 && p.g < 70 && p.b < 70)
            .count()
            > frame.as_slice().len() / 2
    );
}

#[test]
fn dark_css_is_honored_and_original_colors_restore_exact_pixels() {
    let html = r#"<style>
      body { background:#fff; color:#111; }
      @media (prefers-color-scheme: dark) { body { background:#18252e; color:#dbeeff; } }
    </style><body style="margin:0"><p>Sender's native dark design</p><p><a href="https://example.com">A link</a></p></body>"#;
    let mut baseline = prepared(html, EmailAppearance::default());
    let original_pixels = pixels(&mut baseline);
    let mut renderer = GpuEmailRenderer::default();
    renderer.set_email(prepared(html, EmailAppearance::default()));
    let document_id = renderer.email.as_ref().unwrap().document.id();
    renderer.select_all();
    let selection = renderer.selected_text();
    assert!(
        selection
            .as_deref()
            .unwrap_or_default()
            .contains("Sender's native dark design")
    );
    assert_eq!(baseline.links.len(), 1);
    renderer.set_appearance(dark());
    renderer.render_cpu_if_needed(520, 900, 1.0).unwrap();
    assert_color(
        colors(renderer.email.as_ref().unwrap(), "body").1,
        Color::from_rgb8(24, 37, 46),
    );
    assert_eq!(renderer.selected_text(), selection);
    assert_eq!(renderer.email.as_ref().unwrap().document.id(), document_id);
    renderer.set_appearance(EmailAppearance::default());
    renderer.render_cpu_if_needed(520, 900, 1.0).unwrap();
    assert_eq!(renderer.selected_text(), selection);
    // Pixel equality excludes the selection highlight by clearing it on both.
    renderer
        .email
        .as_mut()
        .unwrap()
        .document
        .clear_text_selection();
    let restored = pixels(renderer.email.as_mut().unwrap());
    assert_eq!(original_pixels.as_bytes(), restored.as_bytes());
    assert_eq!(
        renderer.email.as_ref().unwrap().links.len(),
        baseline.links.len()
    );
}

#[test]
fn image_pixels_and_text_over_background_images_keep_authored_colors() {
    let mut image = image::RgbaImage::from_pixel(32, 32, image::Rgba([255, 112, 32, 255]));
    image.put_pixel(0, 0, image::Rgba([16, 255, 160, 255]));
    let mut data = std::io::Cursor::new(Vec::new());
    image.write_to(&mut data, image::ImageFormat::Png).unwrap();
    use base64::Engine;
    let url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(data.into_inner())
    );
    let html = format!(
        r#"<body style="margin:0;background:#fff"><img src="{url}" width="32" height="32"><div id="photo" style="background:white url('{url}');color:#111"><span id="caption">Caption</span></div></body>"#
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut renderer = GpuEmailRenderer::default();
    renderer
        .configure_resources(runtime.handle().clone(), false)
        .unwrap();
    renderer.set_email(renderer.prepare_email_html(&html, false).unwrap());
    let mut loaded = false;
    for _ in 0..100 {
        runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(5)).await });
        renderer.poll_resources();
        let document = &renderer.email.as_ref().unwrap().document;
        let image_id = document.query_selector("img").unwrap().unwrap();
        let photo_id = document.query_selector("#photo").unwrap().unwrap();
        loaded = document
            .get_node(image_id)
            .unwrap()
            .element_data()
            .unwrap()
            .raster_image_data()
            .is_some()
            && document
                .get_node(photo_id)
                .unwrap()
                .element_data()
                .unwrap()
                .background_images
                .iter()
                .any(Option::is_some);
        if loaded {
            break;
        }
    }
    assert!(
        loaded,
        "both embedded images must decode before comparing pixels"
    );
    let email = renderer.email.as_mut().unwrap();
    let photo = colors(email, "#photo");
    let caption = colors(email, "#caption").0;
    let light = pixels(email);
    assert!(
        light
            .as_slice()
            .iter()
            .any(|p| p.r == 255 && p.g == 112 && p.b == 32),
        "decoded orange pixels must actually be painted"
    );
    renderer.set_appearance(dark());
    let email = renderer.email.as_mut().unwrap();
    let dark = pixels(email);
    assert_color(photo.0, colors(email, "#photo").0);
    assert_color(photo.1, colors(email, "#photo").1);
    assert_color(caption, colors(email, "#caption").0);
    for y in 0..32 {
        for x in 0..32 {
            assert_eq!(light.as_slice()[y * 520 + x], dark.as_slice()[y * 520 + x]);
        }
    }
}

#[test]
fn explicit_light_and_custom_palette_use_the_selected_surface() {
    let appearance = EmailAppearance {
        original: false,
        dark: false,
        background: Color::from_rgb8(255, 248, 224),
        foreground: Color::from_rgb8(48, 40, 24),
        ..EmailAppearance::default()
    };
    let mut email = prepared(
        "<body style='margin:0;background:#111;color:white'><p>Light message</p></body>",
        appearance,
    );
    let (fg, bg) = colors(&email, "body");
    assert!(contrast(fg, bg) >= 4.5);
    let frame = pixels(&mut email);
    let canvas = frame.as_slice().last().unwrap();
    assert!(
        canvas.r > 200 && canvas.g > 200,
        "canvas should follow the light palette"
    );
}

#[test]
fn appearance_changes_invalidate_tiles_once_and_keep_document_selection_and_links() {
    let mut renderer = GpuEmailRenderer::default();
    renderer.set_email(prepared(
        "<body><p>Hello <a href='https://example.com'>there</a></p></body>",
        EmailAppearance::default(),
    ));
    renderer.render_cpu_if_needed(520, 900, 1.0).unwrap();
    let before = renderer.tile_count;
    let document_id = renderer.email.as_ref().unwrap().document.id();
    renderer.set_visible_region(42.0, 300.0);
    assert!(renderer.set_appearance(dark()));
    assert!(
        renderer
            .render_cpu_if_needed(520, 900, 1.0)
            .unwrap()
            .is_some()
    );
    assert_eq!(renderer.email.as_ref().unwrap().document.id(), document_id);
    assert_eq!(renderer.visible_scroll_y, 42.0);
    assert!(renderer.tile_count > before);
    assert!(!renderer.set_appearance(dark()));
    assert!(
        renderer
            .render_cpu_if_needed(520, 900, 1.0)
            .unwrap()
            .is_none()
    );
    assert!(!renderer.email.as_ref().unwrap().links.is_empty());
}

#[test]
fn native_color_scheme_changes_refresh_reader_text_and_link_metadata() {
    let html = r#"<style>
        .dark { display:none; } .light { display:block; }
        @media (prefers-color-scheme:dark) { .light { display:none; } .dark { display:block; } }
    </style><body><p class='light'>Light version</p><p class='dark'><a href='https://example.com/dark'>Dark version</a></p></body>"#;
    let mut renderer = GpuEmailRenderer::default();
    renderer.set_email(prepared(html, EmailAppearance::default()));
    renderer.render_cpu_if_needed(520, 900, 1.0).unwrap();
    assert_eq!(renderer.plain_text(), "Light version");
    renderer.set_appearance(dark());
    renderer.render_cpu_if_needed(520, 900, 1.0).unwrap();
    assert_eq!(renderer.email.as_ref().unwrap().plain_text, "Dark version");
    assert_eq!(renderer.plain_text(), "Dark version");
    assert_eq!(
        renderer.email.as_ref().unwrap().links[0].url,
        "https://example.com/dark"
    );
    assert!(
        renderer
            .reader_items()
            .iter()
            .any(|item| item.name.contains("Dark version"))
    );
    renderer.set_appearance(EmailAppearance::default());
    renderer.render_cpu_if_needed(520, 900, 1.0).unwrap();
    assert_eq!(renderer.email.as_ref().unwrap().plain_text, "Light version");
    assert!(renderer.email.as_ref().unwrap().links.is_empty());
}

#[test]
fn a_custom_light_palette_replaces_white_even_when_the_surface_is_saturated() {
    let appearance = EmailAppearance {
        original: false,
        dark: false,
        background: Color::from_rgb8(36, 70, 91),
        foreground: Color::WHITE,
        ..EmailAppearance::default()
    };
    let mut email = prepared(
        "<body style='margin:0;background:white;color:black'><p>Custom colors</p></body>",
        appearance,
    );
    let (fg, bg) = colors(&email, "body");
    assert_color(bg, appearance.background);
    assert!(contrast(fg, bg) >= 4.5);
    let frame = pixels(&mut email);
    let canvas = frame.as_slice().last().unwrap();
    assert_eq!((canvas.r, canvas.g, canvas.b, canvas.a), (36, 70, 91, 255));
}
