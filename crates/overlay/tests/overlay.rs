//! End-to-end overlay behaviour on a synthetic capture: the original glyphs
//! must disappear, the translation must appear inside the same box, and
//! nothing outside the box may be touched.

use cosmic_screenshot_overlay::{
    Canvas, FontSet, Options, Rect, Script, TranslatedBlock, cover_rect, overlay,
};

fn latin() -> FontSet {
    FontSet::system(Script::Latin).expect("a Latin system font is installed")
}

/// White capture with a black bar standing in for a line of text.
fn capture_with_text() -> Canvas {
    let mut canvas = Canvas::filled(300, 140, [252, 252, 252, 255]);
    for y in 50..62 {
        for x in 40..240 {
            canvas.set_pixel(x, y, [12, 12, 12, 255]);
        }
    }
    canvas
}

fn block(text: &str) -> TranslatedBlock {
    TranslatedBlock {
        rect: Rect::new(38.0, 46.0, 206.0, 20.0),
        text: text.to_string(),
    }
}

fn dark_pixels(canvas: &Canvas, rect: Rect) -> usize {
    let mut count = 0;
    for y in rect.y as u32..rect.bottom() as u32 {
        for x in rect.x as u32..rect.right() as u32 {
            let pixel = canvas.pixel(x, y);
            let luminance =
                0.2126 * pixel[0] as f32 + 0.7152 * pixel[1] as f32 + 0.0722 * pixel[2] as f32;
            if luminance < 120.0 {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn empty_translation_covers_the_original_glyphs() {
    let mut canvas = capture_with_text();
    assert!(dark_pixels(&canvas, Rect::new(38.0, 46.0, 206.0, 20.0)) > 1000);

    let report = overlay(&mut canvas, &[block("")], &latin(), &Options::default());

    assert_eq!(report.covered_only, 1);
    assert_eq!(report.drawn, 0);
    assert_eq!(canvas.pixel(100, 55), [252, 252, 252, 255]);
    assert_eq!(dark_pixels(&canvas, Rect::new(38.0, 46.0, 206.0, 20.0)), 0);
}

#[test]
fn draws_the_translation_where_the_original_was() {
    let mut canvas = capture_with_text();
    let report = overlay(
        &mut canvas,
        &[block("Bonjour tout le monde")],
        &latin(),
        &Options::default(),
    );

    assert_eq!(report.drawn, 1);
    assert_eq!(report.skipped, 0);
    assert!(report.max_font_px >= 8.0);

    // Ink inside the box, and it is not the old bar any more: the covered
    // pixels are paper-coloured except where glyphs were drawn.
    let inked = dark_pixels(&canvas, Rect::new(38.0, 46.0, 206.0, 20.0));
    assert!(inked > 10, "expected glyph pixels, found {inked}");
    assert!(
        inked < 900,
        "the cover should have removed the original bar"
    );
}

#[test]
fn nothing_outside_the_box_changes() {
    let original = capture_with_text();
    let mut canvas = capture_with_text();
    let block = block("Bonjour tout le monde");
    let cover = cover_rect(
        block.rect,
        &Options::default(),
        original.width,
        original.height,
    );
    overlay(&mut canvas, &[block], &latin(), &Options::default());

    for y in 0..original.height {
        for x in 0..original.width {
            let inside = cover.contains(x as f32, y as f32);
            if !inside {
                assert_eq!(
                    original.pixel(x, y),
                    canvas.pixel(x, y),
                    "pixel ({x}, {y}) outside the covered box changed"
                );
            }
        }
    }
}

#[test]
fn long_text_is_clipped_to_its_box() {
    let mut canvas = capture_with_text();
    let original = capture_with_text();
    let small = TranslatedBlock {
        rect: Rect::new(40.0, 48.0, 60.0, 12.0),
        text: "Donaudampfschifffahrtsgesellschaft".to_string(),
    };
    let cover = cover_rect(small.rect, &Options::default(), canvas.width, canvas.height);
    overlay(&mut canvas, &[small], &latin(), &Options::default());

    // Nothing right of the small box may be painted, even though the text is
    // far too long for it: the rest of the capture (the old bar) is untouched.
    for x in cover.right() as u32..canvas.width {
        for y in 0..canvas.height {
            assert_eq!(
                canvas.pixel(x, y),
                original.pixel(x, y),
                "painted outside the box at x={x} y={y}"
            );
        }
    }
    assert!(
        dark_pixels(&canvas, cover) > 0,
        "text was clipped away entirely"
    );
}

#[test]
fn blocks_outside_the_image_are_skipped() {
    let mut canvas = Canvas::filled(20, 20, [255, 255, 255, 255]);
    let outside = TranslatedBlock {
        rect: Rect::new(500.0, 500.0, 50.0, 20.0),
        text: "nope".to_string(),
    };
    let report = overlay(&mut canvas, &[outside], &latin(), &Options::default());
    assert_eq!(report.skipped, 1);
    assert_eq!(report.drawn, 0);
}
