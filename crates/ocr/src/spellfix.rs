//! A very small, conservative corrector for OCR of English game menus.
//!
//! A pixel font at 2× confuses `B`/`G`/`F` (`Gack` for `Back`, `Fieport` for
//! `Report`), and the translator then faithfully mangles the nonsense. This
//! module repairs a token only when it is genuinely close to a word the UI is
//! likely to use, and only when the choice is unambiguous:
//!
//! * one edit from a known UI word, for a token of at least four letters;
//! * two edits only when an adjacent token corroborates a known phrase
//!   (`game menu`, `report bugs`, …) — so `bane` becomes `Game` rather than
//!   `Back`, and a random four-letter fragment is left alone;
//! * tokens already in the vocabulary, and tokens shorter than four letters,
//!   are never touched (`zu` in a German menu must stay `zu`).
//!
//! Nothing here invents text: a token is either unchanged or replaced by a
//! vocabulary word, and every replacement is reported so the run log can show
//! when the corrector fired.
//!
//! The corrector is also **English-gated**: it only runs on a line that already
//! contains an exact vocabulary word that is not shared with German (`OK`,
//! `Sound`, `Video`, `Feedback`, `Credits`, `Mods` do not count). A German line
//! (`Musik`, `Menü`, `Optionen`) has no such anchor, so its real words are never
//! rewritten into English look-alikes.

/// A token the corrector replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Correction {
    pub from: String,
    pub to: String,
    /// Levenshtein distance between the two, 1 or 2.
    pub distance: usize,
}

/// Words the UI vocabulary is built from. Deliberately small: menu labels,
/// their common verbs, and the settings screens they open.
const UI_WORDS: &[&str] = &[
    "achievements",
    "advancements",
    "apply",
    "back",
    "bugs",
    "cancel",
    "close",
    "continue",
    "controls",
    "credits",
    "delete",
    "disconnect",
    "exit",
    "feedback",
    "game",
    "give",
    "language",
    "load",
    "menu",
    "mods",
    "multiplayer",
    "music",
    "new",
    "ok",
    "open",
    "options",
    "play",
    "quit",
    "report",
    "reset",
    "resume",
    "save",
    "select",
    "settings",
    "singleplayer",
    "sound",
    "statistics",
    "video",
    "world",
];

/// Two-word phrases that settle a tie between equally close candidates.
const PHRASES: &[(&str, &str)] = &[
    ("back", "to"),
    ("game", "menu"),
    ("give", "feedback"),
    ("report", "bugs"),
];

/// Vocabulary words that are also ordinary German (or otherwise shared) words,
/// so their presence does not prove the line is English. `game` stays an anchor:
/// the owner's `Gack to Game` depends on it, and it is a loanword rather than a
/// native German label.
const AMBIGUOUS_ANCHORS: [&str; 6] = ["ok", "sound", "video", "feedback", "credits", "mods"];

/// Tokens shorter than this are never corrected: `zu`, `to`, `OK` are real
/// words in one source language or another, and one edit would rewrite them.
const MIN_LENGTH: usize = 4;

/// The longest edit distance considered at all.
const MAX_DISTANCE: usize = 2;

/// Correct a line's tokens, in reading order.
///
/// The result has one entry per input token; `corrections` lists only the
/// tokens that changed, in the order they were repaired.
pub fn correct_line(tokens: &[&str]) -> (Vec<String>, Vec<Correction>) {
    let mut corrected: Vec<String> = tokens.iter().map(|token| token.to_string()).collect();
    let mut corrections = Vec::new();

    // English-gate: a line with no exact UI word is probably not English, and
    // rewriting `Musik` or `Menü` into an English look-alike would be wrong.
    // Words that are also German (`OK`, `Sound`, `Video`, …) cannot prove the
    // line is English and are not anchors.
    let anchored = tokens.iter().any(|token| {
        let lower = token.to_lowercase();
        UI_WORDS.contains(&lower.as_str()) && !AMBIGUOUS_ANCHORS.contains(&lower.as_str())
    });
    if !anchored {
        return (corrected, corrections);
    }

    for (position, token) in tokens.iter().enumerate() {
        if !is_plain_word(token) || token.chars().count() < MIN_LENGTH {
            continue;
        }
        let lower = token.to_lowercase();
        if UI_WORDS.contains(&lower.as_str()) {
            continue;
        }

        let mut candidates: Vec<(&'static str, usize)> = UI_WORDS
            .iter()
            .filter_map(|word| {
                let distance = edit_distance(&lower, word, MAX_DISTANCE);
                (distance <= MAX_DISTANCE).then_some((*word, distance))
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        candidates.sort_by_key(|(word, distance)| (*distance, *word));

        let best = candidates[0].1;
        let nearest: Vec<&'static str> = candidates
            .iter()
            .filter(|(_, distance)| *distance == best)
            .map(|(word, _)| *word)
            .collect();

        let picked = if best == 1 && nearest.len() == 1 {
            Some(nearest[0])
        } else {
            // Several equally close words, or a two-edit match: only a known
            // phrase with a neighbour may settle it.
            let mut corroborated: Vec<&'static str> = nearest
                .iter()
                .copied()
                .filter(|word| corroborated_by_phrase(tokens, position, word))
                .collect();
            corroborated.sort_unstable();
            corroborated.dedup();
            (corroborated.len() == 1).then(|| corroborated[0])
        };

        if let Some(word) = picked {
            let replacement = match_case(token, word);
            corrections.push(Correction {
                from: token.to_string(),
                to: replacement.clone(),
                distance: best,
            });
            corrected[position] = replacement;
        }
    }

    (corrected, corrections)
}

/// Only all-alphabetic tokens are safe to rewrite; `Options...` and `|` keep
/// their punctuation and their box.
fn is_plain_word(token: &str) -> bool {
    !token.is_empty() && token.chars().all(char::is_alphabetic)
}

/// Does the token before or after this one form a known phrase with `word`?
fn corroborated_by_phrase(tokens: &[&str], position: usize, word: &str) -> bool {
    let neighbour = |offset: isize| -> Option<String> {
        let index = position as isize + offset;
        if index < 0 {
            return None;
        }
        tokens.get(index as usize).map(|token| token.to_lowercase())
    };
    let left = neighbour(-1);
    let right = neighbour(1);
    PHRASES.iter().any(|(first, second)| {
        (*first == word && right.as_deref() == Some(*second))
            || (*second == word && left.as_deref() == Some(*first))
    })
}

/// Keep the token's capitalisation: a lower-case misread stays lower case, a
/// title-cased one stays title-cased.
fn match_case(original: &str, canonical: &str) -> String {
    if original.chars().count() > 1 && original.chars().all(|character| character.is_uppercase()) {
        return canonical.to_uppercase();
    }
    let mut characters = original.chars();
    let title_cased = characters.next().is_some_and(|first| first.is_uppercase());
    if title_cased {
        let mut characters = canonical.chars();
        match characters.next() {
            Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
            None => String::new(),
        }
    } else {
        canonical.to_string()
    }
}

/// Levenshtein distance, abandoned once it exceeds `limit`.
fn edit_distance(left: &str, right: &str, limit: usize) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len().abs_diff(right.len()) > limit {
        return limit + 1;
    }

    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (row, left_character) in left.iter().enumerate() {
        current[0] = row + 1;
        let mut row_minimum = current[0];
        for (column, right_character) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_character != right_character);
            let insertion = current[column] + 1;
            let deletion = previous[column + 1] + 1;
            current[column + 1] = substitution.min(insertion).min(deletion);
            row_minimum = row_minimum.min(current[column + 1]);
        }
        if row_minimum > limit {
            return limit + 1;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corrected(tokens: &[&str]) -> Vec<String> {
        correct_line(tokens).0
    }

    #[test]
    fn one_edit_typos_are_repaired() {
        assert_eq!(corrected(&["Gack", "to", "Game"]), ["Back", "to", "Game"]);
        assert_eq!(corrected(&["Disconect", "Game"]), ["Disconnect", "Game"]);
    }

    #[test]
    fn a_line_without_an_english_anchor_is_left_alone() {
        // Real words in other languages must not be rewritten into English
        // look-alikes; none of these lines contains an unambiguous English word.
        assert_eq!(corrected(&["Musik", "Lautstärke"]), ["Musik", "Lautstärke"]);
        assert_eq!(corrected(&["Menü", "Optionen"]), ["Menü", "Optionen"]);
        assert_eq!(corrected(&["Musica"]), ["Musica"]);
        assert_eq!(corrected(&["Disconect"]), ["Disconect"]);
        // These words exist in German too, so they are not anchors.
        assert_eq!(corrected(&["OK", "Menü"]), ["OK", "Menü"]);
        assert_eq!(corrected(&["Sound", "Musik"]), ["Sound", "Musik"]);
        assert_eq!(corrected(&["Video", "Musik"]), ["Video", "Musik"]);
    }

    #[test]
    fn a_two_edit_typo_needs_a_known_phrase() {
        // `bane` is two edits from Back, Game and Save; `menu` picks Game.
        assert_eq!(corrected(&["Bane", "Menu"]), ["Game", "Menu"]);
        assert_eq!(corrected(&["bane", "Menu"]), ["game", "Menu"]);
        assert_eq!(corrected(&["Fieport", "Bugs"]), ["Report", "Bugs"]);
        // No phrase, no guess.
        assert_eq!(corrected(&["bane"]), ["bane"]);
        assert_eq!(corrected(&["Lact"]), ["Lact"]);
    }

    #[test]
    fn case_is_preserved() {
        assert_eq!(corrected(&["GACK", "Game"]), ["BACK", "Game"]);
        assert_eq!(corrected(&["gack", "Game"]), ["back", "Game"]);
        assert_eq!(corrected(&["Gack", "Game"]), ["Back", "Game"]);
    }

    #[test]
    fn real_short_words_and_vocabulary_words_are_left_alone() {
        assert_eq!(
            corrected(&["zu", "to", "OK", "Menu"]),
            ["zu", "to", "OK", "Menu"]
        );
        // `Menu` is already a UI word; it is not rewritten.
        assert_eq!(correct_line(&["Menu"]).1.len(), 0);
    }

    #[test]
    fn punctuation_is_never_rewritten() {
        assert_eq!(corrected(&["Options...", "Game"]), ["Options...", "Game"]);
        assert_eq!(corrected(&["|", "Game"]), ["|", "Game"]);
    }

    #[test]
    fn corrections_are_reported() {
        let (_, corrections) = correct_line(&["Gack", "Fieport", "Bugs"]);
        assert_eq!(
            corrections,
            vec![
                Correction {
                    from: "Gack".to_string(),
                    to: "Back".to_string(),
                    distance: 1,
                },
                Correction {
                    from: "Fieport".to_string(),
                    to: "Report".to_string(),
                    distance: 2,
                },
            ]
        );
    }

    #[test]
    fn distance_is_bounded() {
        assert_eq!(edit_distance("game", "game", 2), 0);
        assert_eq!(edit_distance("gack", "back", 2), 1);
        assert_eq!(edit_distance("bane", "game", 2), 2);
        assert_eq!(edit_distance("fieport", "report", 2), 2);
        assert_eq!(edit_distance("banana", "game", 2), 3);
    }
}
