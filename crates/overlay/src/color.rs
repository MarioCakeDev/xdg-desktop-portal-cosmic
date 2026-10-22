//! Sampling the original pixels so the cover and the replacement text match
//! what was there: paper colour around the box, ink colour inside it.

use crate::{Canvas, Rect};

/// Rec. 709 relative luminance, 0–255 scale.
pub fn luminance(pixel: [u8; 4]) -> f32 {
    0.2126 * pixel[0] as f32 + 0.7152 * pixel[1] as f32 + 0.0722 * pixel[2] as f32
}

/// Median colour of the ring just outside `rect` — the "paper" behind the text.
/// A median (not a mean) keeps nearby glyphs and borders from tinting the cover.
pub fn background(canvas: &Canvas, rect: Rect) -> [u8; 4] {
    let outer = rect.expand(3.0).clamp(canvas.width, canvas.height);
    let mut red = Vec::new();
    let mut green = Vec::new();
    let mut blue = Vec::new();

    let left = outer.x.floor() as u32;
    let top = outer.y.floor() as u32;
    let right = outer.right().ceil() as u32;
    let bottom = outer.bottom().ceil() as u32;
    let stride = stride_for(outer);

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
                    red.push(pixel[0]);
                    green.push(pixel[1]);
                    blue.push(pixel[2]);
                }
            }
            x += stride;
        }
        y += stride;
    }

    if red.is_empty() {
        return [255, 255, 255, 255];
    }
    [median(&mut red), median(&mut green), median(&mut blue), 255]
}

/// Median colour of the pixels inside `rect` that differ from the background,
/// i.e. the original text colour.
pub fn foreground(canvas: &Canvas, rect: Rect, background: [u8; 4], min_contrast: f32) -> [u8; 4] {
    let background_luminance = luminance(background);
    let left = rect.x.floor().max(0.0) as u32;
    let top = rect.y.floor().max(0.0) as u32;
    let right = rect.right().ceil().min(canvas.width as f32) as u32;
    let bottom = rect.bottom().ceil().min(canvas.height as f32) as u32;
    let stride = stride_for(rect);

    let mut red = Vec::new();
    let mut green = Vec::new();
    let mut blue = Vec::new();

    let mut y = top;
    while y < bottom {
        let mut x = left;
        while x < right {
            let pixel = canvas.pixel(x, y);
            if pixel[3] > 8 && (luminance(pixel) - background_luminance).abs() >= min_contrast {
                red.push(pixel[0]);
                green.push(pixel[1]);
                blue.push(pixel[2]);
            }
            x += stride;
        }
        y += stride;
    }

    if red.is_empty() {
        // No readable ink (blank or gradient box): pick the readable extreme.
        return if background_luminance > 128.0 {
            [16, 16, 16, 255]
        } else {
            [245, 245, 245, 255]
        };
    }
    [median(&mut red), median(&mut green), median(&mut blue), 255]
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

fn median(values: &mut [u8]) -> u8 {
    values.sort_unstable();
    values[values.len() / 2]
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
}
