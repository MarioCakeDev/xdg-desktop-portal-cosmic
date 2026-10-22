//! Sampling the original pixels so the cover and the replacement text match
//! what was there: paper colour around the box, ink colour inside it.
//!
//! Two properties matter more than fidelity to a local average:
//!
//! - The cover must be **one of the colours that was already there**. A box
//!   that straddles a selected block and the page around it has two flat paper
//!   colours, and their *median* is a third, mid-grey one that exists nowhere
//!   on screen. The translated text then sits on a cover it has almost no
//!   contrast with. The dominant (modal) colour of the ring is one of the two.
//! - The ink/fill pair must **read**. Whatever we sampled, if the pair is below
//!   [`MIN_TEXT_CONTRAST`] the ink becomes the readable extreme; if even that
//!   cannot contrast with the fill, the fill itself moves away from the ink.

use crate::{Canvas, Rect};

/// Rec. 709 relative luminance, 0–255 scale.
pub fn luminance(pixel: [u8; 4]) -> f32 {
    0.2126 * pixel[0] as f32 + 0.7152 * pixel[1] as f32 + 0.0722 * pixel[2] as f32
}

/// WCAG 2 contrast ratio for two colours, 1.0 (identical) to 21.0 (black/white).
pub fn contrast_ratio(left: [u8; 4], right: [u8; 4]) -> f32 {
    fn linear(value: u8) -> f32 {
        let value = value as f32 / 255.0;
        if value <= 0.039_28 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    fn relative(pixel: [u8; 4]) -> f32 {
        0.2126 * linear(pixel[0]) + 0.7152 * linear(pixel[1]) + 0.0722 * linear(pixel[2])
    }
    let (left, right) = (relative(left), relative(right));
    let (brighter, darker) = if left >= right {
        (left, right)
    } else {
        (right, left)
    };
    (brighter + 0.05) / (darker + 0.05)
}

/// Body-text contrast, WCAG AA. Below this the replacement is not readable.
pub const MIN_TEXT_CONTRAST: f32 = 4.5;

/// Darkest and lightest ink the overlay falls back to.
const DARK_INK: [u8; 4] = [16, 16, 16, 255];
const LIGHT_INK: [u8; 4] = [245, 245, 245, 255];

/// The cover colour and the ink for one block, guaranteed readable.
///
/// This is what [`crate::overlay`] draws with: [`background`] for the fill,
/// [`foreground`] for the ink, then the pair enforced against
/// [`MIN_TEXT_CONTRAST`].
pub fn pair(canvas: &Canvas, rect: Rect, min_contrast: f32) -> ([u8; 4], [u8; 4]) {
    let mut fill = background(canvas, rect);
    let mut ink = foreground(canvas, rect, fill, min_contrast);

    if contrast_ratio(fill, ink) < MIN_TEXT_CONTRAST {
        // The sampled ink was too close to the fill — a box over two papers, or
        // a capture whose own text is that low-contrast. Take the readable
        // extreme instead of painting the translation invisible.
        let extreme = readable_extreme(fill);
        if contrast_ratio(fill, extreme) > contrast_ratio(fill, ink) {
            ink = extreme;
        }
    }
    if contrast_ratio(fill, ink) < MIN_TEXT_CONTRAST {
        // Even black or white cannot reach the ratio: the fill is mid-grey and
        // is the problem, so move it away from the ink until the pair reads.
        fill = separate(fill, ink);
    }
    (fill, ink)
}

/// Dominant (modal) colour of the ring just outside `rect` — the "paper"
/// behind the text.
///
/// A mode, not a median: a ring that spans a highlight and the page has two
/// colours, and their median is a mid-grey that is on screen nowhere. Buckets
/// are 16 luma levels wide per channel so antialiasing and JPEG noise do not
/// split one paper across many near-colours; the returned pixel is the average
/// of the winning bucket.
pub fn background(canvas: &Canvas, rect: Rect) -> [u8; 4] {
    let outer = rect.expand(3.0).clamp(canvas.width, canvas.height);
    let left = outer.x.floor() as u32;
    let top = outer.y.floor() as u32;
    let right = outer.right().ceil() as u32;
    let bottom = outer.bottom().ceil() as u32;
    let stride = stride_for(outer);

    // 16 levels per channel: 4096 buckets of (count, red, green, blue sums).
    let mut buckets = [[0u32; 4]; 4096];
    let mut sampled = 0u32;

    let mut y = top;
    while y < bottom {
        let mut x = left;
        while x < right {
            let inside = x as f32 >= rect.x
                && x as f32 <= rect.right()
                && y as f32 >= rect.y
                && y as f32 <= rect.bottom();
            if !inside {
                let pixel = canvas.pixel(x, y);
                if pixel[3] > 8 {
                    let bucket = ((pixel[0] as usize >> 4) << 8)
                        | ((pixel[1] as usize >> 4) << 4)
                        | (pixel[2] as usize >> 4);
                    let counts = &mut buckets[bucket];
                    counts[0] += 1;
                    counts[1] += pixel[0] as u32;
                    counts[2] += pixel[1] as u32;
                    counts[3] += pixel[2] as u32;
                    sampled += 1;
                }
            }
            x += stride;
        }
        y += stride;
    }

    if sampled == 0 {
        return [255, 255, 255, 255];
    }
    // Ties go to the darker bucket, so the choice is deterministic.
    let mut winner = 0;
    for (index, counts) in buckets.iter().enumerate() {
        if counts[0] > buckets[winner][0] {
            winner = index;
        }
    }
    let counts = buckets[winner];
    let total = counts[0].max(1);
    [
        (counts[1] / total) as u8,
        (counts[2] / total) as u8,
        (counts[3] / total) as u8,
        255,
    ]
}

/// The colour of the original text inside `rect`: the pixels that differ from
/// `background`, taken at the end of the range that faces away from the paper.
///
/// A median would include the antialiased edge and drag the ink towards the
/// paper (a drawn translation used to peak at 168 where the page's own text
/// peaked at 230). The tenth percentile from the far side is the glyph core.
pub fn foreground(canvas: &Canvas, rect: Rect, background: [u8; 4], min_contrast: f32) -> [u8; 4] {
    let background_luminance = luminance(background);
    let left = rect.x.floor().max(0.0) as u32;
    let top = rect.y.floor().max(0.0) as u32;
    let right = rect.right().ceil().min(canvas.width as f32) as u32;
    let bottom = rect.bottom().ceil().min(canvas.height as f32) as u32;
    let stride = stride_for(rect);

    let mut pixels: Vec<[u8; 4]> = Vec::new();
    let mut y = top;
    while y < bottom {
        let mut x = left;
        while x < right {
            let pixel = canvas.pixel(x, y);
            if pixel[3] > 8 && (luminance(pixel) - background_luminance).abs() >= min_contrast {
                pixels.push(pixel);
            }
            x += stride;
        }
        y += stride;
    }

    if pixels.is_empty() {
        return readable_extreme(background);
    }
    pixels.sort_by(|left, right| {
        luminance(*left)
            .partial_cmp(&luminance(*right))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let middle = luminance(pixels[pixels.len() / 2]);
    let index = if middle < background_luminance {
        pixels.len() / 10
    } else {
        (pixels.len() * 9) / 10
    };
    let mut ink = pixels[index];
    ink[3] = 255;
    ink
}

/// Black on light paper, white on dark paper.
fn readable_extreme(paper: [u8; 4]) -> [u8; 4] {
    if luminance(paper) > 128.0 {
        DARK_INK
    } else {
        LIGHT_INK
    }
}

/// Push `fill` away from `ink` (towards white when the ink is dark) until the
/// pair reaches [`MIN_TEXT_CONTRAST`]. Only a mid-grey paper ever gets here.
fn separate(fill: [u8; 4], ink: [u8; 4]) -> [u8; 4] {
    let towards_white = luminance(ink) < 128.0;
    let target = if towards_white { 255.0 } else { 0.0 };
    let mut candidate = fill;
    for step in 1..=16 {
        let amount = step as f32 / 16.0;
        let channel = |value: u8| {
            let value = value as f32;
            (value + (target - value) * amount).round() as u8
        };
        candidate = [channel(fill[0]), channel(fill[1]), channel(fill[2]), 255];
        if contrast_ratio(candidate, ink) >= MIN_TEXT_CONTRAST {
            break;
        }
    }
    candidate
}

/// Keep sampling bounded for large boxes.
fn stride_for(rect: Rect) -> u32 {
    let pixels = rect.width.max(1.0) * rect.height.max(1.0);
    if pixels > 40_000.0 {
        3
    } else if pixels > 8_000.0 {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas_with_text() -> Canvas {
        // White paper with a black horizontal bar in the middle of the box.
        let mut canvas = Canvas::filled(100, 60, [250, 250, 250, 255]);
        for y in 25..35 {
            for x in 20..80 {
                canvas.set_pixel(x, y, [10, 10, 10, 255]);
            }
        }
        canvas
    }

    #[test]
    fn background_is_the_paper_not_the_ink() {
        let canvas = canvas_with_text();
        let rect = Rect::new(20.0, 20.0, 60.0, 20.0);
        let background = background(&canvas, rect);
        assert!(background[0] > 230, "{background:?}");
    }

    #[test]
    fn foreground_is_the_ink() {
        let canvas = canvas_with_text();
        let rect = Rect::new(20.0, 20.0, 60.0, 20.0);
        let background = background(&canvas, rect);
        let foreground = foreground(&canvas, rect, background, 40.0);
        assert!(foreground[0] < 40, "{foreground:?}");
    }

    #[test]
    fn blank_box_falls_back_to_black_on_white() {
        let canvas = Canvas::filled(20, 20, [255, 255, 255, 255]);
        let rect = Rect::new(2.0, 2.0, 10.0, 10.0);
        let background = background(&canvas, rect);
        let foreground = foreground(&canvas, rect, background, 40.0);
        assert_eq!(foreground, [16, 16, 16, 255]);
    }

    #[test]
    fn transparent_pixels_are_ignored() {
        let mut canvas = Canvas::filled(20, 20, [255, 255, 255, 255]);
        canvas.set_pixel(5, 5, [0, 0, 0, 0]);
        let rect = Rect::new(4.0, 4.0, 4.0, 4.0);
        assert_eq!(background(&canvas, rect), [255, 255, 255, 255]);
    }

    #[test]
    fn a_ring_over_two_papers_picks_one_of_them_not_the_blend() {
        // The defect in miniature: a box straddling a selected block (light)
        // and the page (dark). The ring is split between the two.
        let mut canvas = Canvas::filled(100, 40, [15, 15, 15, 255]);
        for y in 0..24 {
            for x in 30..70 {
                canvas.set_pixel(x, y, [207, 228, 245, 255]);
            }
        }
        // Ink of the highlighted line, inside the box.
        for y in 6..8 {
            for x in 32..68 {
                canvas.set_pixel(x, y, [20, 20, 20, 255]);
            }
        }
        let rect = Rect::new(32.0, 5.0, 36.0, 10.0);
        let fill = background(&canvas, rect);
        assert_eq!(
            fill,
            [207, 228, 245, 255],
            "the cover must be the highlight, not a mid-grey blend of it and the page"
        );
        let (fill, ink) = pair(&canvas, rect, 40.0);
        assert!(
            contrast_ratio(fill, ink) >= MIN_TEXT_CONTRAST,
            "{fill:?} {ink:?}"
        );
    }

    #[test]
    fn a_light_ink_on_a_mid_grey_cover_is_made_readable() {
        // Run-5's invisible line: light glyphs picked as ink on a mid-grey
        // cover. The pair has to reach AA whatever was sampled.
        let mut canvas = Canvas::filled(60, 40, [128, 128, 128, 255]);
        for y in 15..25 {
            for x in 10..50 {
                canvas.set_pixel(x, y, [210, 210, 210, 255]);
            }
        }
        let rect = Rect::new(10.0, 12.0, 40.0, 16.0);
        let (fill, ink) = pair(&canvas, rect, 40.0);
        assert!(
            contrast_ratio(fill, ink) >= MIN_TEXT_CONTRAST,
            "fill {fill:?} ink {ink:?} is {}:1",
            contrast_ratio(fill, ink)
        );
    }

    #[test]
    fn the_ink_is_the_glyph_core_not_its_antialiased_edge() {
        // A soft ramp from paper to glyph: the median of the "not paper"
        // pixels lands on the edge and used to paint a dim translation.
        let mut canvas = Canvas::filled(80, 30, [12, 12, 12, 255]);
        for x in 10..70 {
            for (offset, value) in [(8, 90u8), (9, 150), (10, 200), (11, 235), (12, 235)] {
                canvas.set_pixel(x, offset, [value, value, value, 255]);
            }
        }
        let rect = Rect::new(10.0, 6.0, 60.0, 12.0);
        let fill = background(&canvas, rect);
        let ink = foreground(&canvas, rect, fill, 40.0);
        assert!(ink[0] > 220, "expected the glyph core, got {ink:?}");
        assert!(contrast_ratio(fill, ink) >= MIN_TEXT_CONTRAST);
    }

    #[test]
    fn a_dark_block_inside_a_light_page_reads_too() {
        // The mirror case: the paper is light, the block's paper is dark.
        let mut canvas = Canvas::filled(100, 40, [250, 250, 250, 255]);
        for y in 14..26 {
            for x in 30..70 {
                canvas.set_pixel(x, y, [18, 18, 18, 255]);
            }
        }
        for y in 18..21 {
            for x in 34..66 {
                canvas.set_pixel(x, y, [225, 225, 225, 255]);
            }
        }
        let rect = Rect::new(33.0, 16.0, 34.0, 8.0);
        let (fill, ink) = pair(&canvas, rect, 40.0);
        assert_eq!(fill, [18, 18, 18, 255]);
        assert!(ink[0] > 200, "{ink:?}");
        assert!(contrast_ratio(fill, ink) >= MIN_TEXT_CONTRAST);
    }

    #[test]
    fn the_ratio_is_wcag_shaped() {
        assert!((contrast_ratio([0, 0, 0, 255], [255, 255, 255, 255]) - 21.0).abs() < 0.01);
        assert!((contrast_ratio([90, 90, 90, 255], [90, 90, 90, 255]) - 1.0).abs() < 0.01);
    }
}
