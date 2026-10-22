//! The capture that showed the defect: a dark page with a **selected passage**
//! in the middle. The passage is a light block with dark glyphs, and the crop's
//! global polarity names the page, so the whole passage was folded into the
//! background by the split and its lines never reached Tesseract.
//!
//! The fixture is synthetic — DejaVu Sans on the page and in the selection — so
//! it can be regenerated on any machine with a font and carries no real page.
//! Tesseract is optional in CI (the portal's test job has no OCR engine), so
//! the test says so and returns instead of failing when it is missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use cosmic_screenshot_ocr::{Config, Reading, recognize_reading};

/// Rows of the selection in the fixture, in capture pixels.
const SELECTION: (u32, u32) = (86, 258);

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/selection-in-dark-page.png")
}

fn tesseract_available() -> bool {
    Command::new("tesseract")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn config() -> Config {
    Config {
        // Only the pack this fixture is written in; the full list is not
        // installed everywhere.
        languages: "eng".to_string(),
        ..Config::default()
    }
}

fn lines_within(reading: &Reading, top: u32, bottom: u32) -> Vec<String> {
    reading
        .blocks
        .iter()
        .filter(|block| {
            let centre = block.bbox.y + block.bbox.height / 2;
            centre >= top && centre < bottom
        })
        .map(|block| block.text.clone())
        .collect()
}

#[test]
fn the_global_polarity_costs_the_selected_lines() {
    if !tesseract_available() {
        eprintln!("skipping: tesseract is not installed");
        return;
    }
    // The defect, kept as a witness: with one polarity for the whole crop, the
    // passage is a solid bar and nothing in it is read.
    let reading = recognize_reading(&fixture(), &config().with_retry_opposite_polarity(false))
        .expect("a reading");
    let passage = lines_within(&reading, SELECTION.0, SELECTION.1);
    assert!(
        passage.is_empty(),
        "the fixture must still show the defect without the retry, read {passage:?}"
    );
}

#[test]
fn the_selected_passage_is_read_by_the_opposite_polarity_pass() {
    if !tesseract_available() {
        eprintln!("skipping: tesseract is not installed");
        return;
    }
    let reading = recognize_reading(&fixture(), &config()).expect("a reading");
    let passage = lines_within(&reading, SELECTION.0, SELECTION.1);
    assert!(
        passage.len() >= 2,
        "the passage must be recovered line by line, got {passage:?} from {:#?}",
        reading.blocks
    );
    assert!(
        reading.recovered >= 2,
        "the recovered lines must be accounted for, got {}",
        reading.recovered
    );

    // And the page's own lines are still read: the retry adds to the primary
    // reading instead of replacing it.
    let above = lines_within(&reading, 0, SELECTION.0);
    let below = lines_within(&reading, SELECTION.1, u32::MAX);
    assert!(
        !above.is_empty() && !below.is_empty(),
        "page lines lost: above {above:?}, below {below:?}"
    );
}
