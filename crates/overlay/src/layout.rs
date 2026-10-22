//! Fitting translated text into the box the original occupied.
//!
//! Word-wrap first (falling back to per-character breaks for scripts without
//! spaces), then shrink the font until the block fits. The size is chosen from
//! the largest that fits so the result looks like a re-render of the original,
//! not a caption.

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};

#[derive(Clone, Debug, PartialEq)]
pub struct Fitted {
    pub size: f32,
    pub lines: Vec<String>,
    pub line_height: f32,
    pub height: f32,
    pub width: f32,
}

/// Advance width of `text` at `size`, including kerning.
pub fn measure(text: &str, font: &FontArc, size: f32) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut previous = None;
    for character in text.chars() {
        let glyph = scaled.glyph_id(character);
        if let Some(previous) = previous {
            width += scaled.kern(previous, glyph);
        }
        width += scaled.h_advance(glyph);
        previous = Some(glyph);
    }
    width
}

/// Greedy word wrap; words too wide for the line are broken by character.
pub fn wrap(text: &str, font: &FontArc, size: f32, max_width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    if max_width <= 0.0 {
        return lines;
    }

    for paragraph in text.split('\n') {
        let words: Vec<&str> = paragraph.split_whitespace().collect();
        let mut current = String::new();

        for word in words {
            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if measure(&candidate, font, size) <= max_width {
                current = candidate;
                continue;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            if measure(word, font, size) <= max_width {
                current = word.to_string();
                continue;
            }
            // A single unbreakable word: break it by character instead of
            // letting it run over the neighbouring text.
            let mut piece = String::new();
            for character in word.chars() {
                let mut next = piece.clone();
                next.push(character);
                if measure(&next, font, size) > max_width && !piece.is_empty() {
                    lines.push(std::mem::take(&mut piece));
                    piece.push(character);
                } else {
                    piece = next;
                }
            }
            current = piece;
        }

        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

/// Largest size at which `text` fits `max_width` × `max_height`.
pub fn fit(
    text: &str,
    font: &FontArc,
    max_width: f32,
    max_height: f32,
    min_size: f32,
    max_size: f32,
    line_height_factor: f32,
) -> Fitted {
    let min_size = min_size.max(1.0);
    let max_size = max_size.max(min_size);
    let factor = line_height_factor.max(0.5);

    let mut size = max_size.floor();
    while size >= min_size {
        if let Some(fitted) = try_size(text, font, size, max_width, max_height, factor) {
            return fitted;
        }
        size -= 1.0;
    }

    // Even the smallest size overflows: draw it clipped rather than nothing.
    let lines = wrap(text, font, min_size, max_width);
    let line_height = min_size * factor;
    let width = lines
        .iter()
        .map(|line| measure(line, font, min_size))
        .fold(0.0_f32, f32::max);
    let height = if lines.len() <= 1 {
        min_size
    } else {
        line_height * (lines.len() - 1) as f32 + min_size
    };
    Fitted {
        size: min_size,
        height,
        lines,
        line_height,
        width,
    }
}

fn try_size(
    text: &str,
    font: &FontArc,
    size: f32,
    max_width: f32,
    max_height: f32,
    factor: f32,
) -> Option<Fitted> {
    let lines = wrap(text, font, size, max_width);
    if lines.is_empty() {
        return Some(Fitted {
            size,
            lines,
            line_height: size * factor,
            height: 0.0,
            width: 0.0,
        });
    }
    let width = lines
        .iter()
        .map(|line| measure(line, font, size))
        .fold(0.0_f32, f32::max);
    let line_height = size * factor;
    // A single line needs no second baseline: its height is one em, not one
    // line advance. This keeps the replacement at the original's scale instead
    // of shrinking it to leave an unused gap.
    let height = if lines.len() <= 1 {
        size
    } else {
        line_height * (lines.len() - 1) as f32 + size
    };

    if width <= max_width + 0.01 && height <= max_height + 0.01 {
        Some(Fitted {
            size,
            lines,
            line_height,
            height,
            width,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FontSet, Script};

    fn font() -> FontSet {
        FontSet::system(Script::Latin).expect("a Latin system font is required for tests")
    }

    #[test]
    fn wraps_at_word_boundaries() {
        let font = font();
        let lines = wrap("the quick brown fox jumps", &font.font, 14.0, 80.0);
        assert!(lines.len() > 1, "{lines:?}");
        for line in &lines {
            assert!(measure(line, &font.font, 14.0) <= 80.5, "{line:?} too wide");
            assert!(!line.starts_with(' ') && !line.ends_with(' '), "{line:?}");
        }
        assert_eq!(lines.join(" "), "the quick brown fox jumps");
    }

    #[test]
    fn breaks_unbreakable_words_by_character() {
        let font = font();
        let lines = wrap("Donaudampfschifffahrtsgesellschaft", &font.font, 14.0, 60.0);
        assert!(lines.len() > 1, "{lines:?}");
        for line in &lines {
            assert!(measure(line, &font.font, 14.0) <= 60.5, "{line:?}");
        }
        assert_eq!(lines.concat(), "Donaudampfschifffahrtsgesellschaft");
    }

    #[test]
    fn fits_long_text_by_shrinking() {
        let font = font();
        let short = fit("Hi", &font.font, 200.0, 40.0, 8.0, 48.0, 1.18);
        let long = fit(
            "This is a considerably longer translation",
            &font.font,
            200.0,
            40.0,
            8.0,
            48.0,
            1.18,
        );
        assert!(short.size > long.size, "{} vs {}", short.size, long.size);
        assert!(long.height <= 40.5, "{long:?}");
        assert!(long.width <= 200.5, "{long:?}");
    }

    #[test]
    fn never_exceeds_the_requested_size() {
        let font = font();
        let fitted = fit("A", &font.font, 500.0, 500.0, 8.0, 32.0, 1.2);
        assert!(fitted.size <= 32.0, "{fitted:?}");
    }
}
