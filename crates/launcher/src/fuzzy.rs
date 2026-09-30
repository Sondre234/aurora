//! Fuzzy subsequence matcher (the fzy scoring scheme) with match positions.
//!
//! Pure and allocation-light: no I/O, no globals. A query matches a candidate when its
//! characters appear in order (case-insensitive). The score rewards consecutive runs and
//! matches at word starts (after space, `-`, `_`, `/`, `.` or a lower-to-upper camel
//! transition) and penalizes gaps, so `ffx` ranks "Firefox" above "Office Fax".
//! Scores are integers in milli-points so ordering is exact and testable.

const SCORE_GAP_LEADING: i32 = -5;
const SCORE_GAP_TRAILING: i32 = -5;
const SCORE_GAP_INNER: i32 = -10;
const SCORE_MATCH_CONSECUTIVE: i32 = 1000;
const SCORE_MATCH_SLASH: i32 = 900;
const SCORE_MATCH_WORD: i32 = 800;
const SCORE_MATCH_CAPITAL: i32 = 700;
const SCORE_MATCH_DOT: i32 = 600;
const NEG: i32 = i32::MIN / 4;

/// Candidates longer than this (in chars) are cut before matching.
pub const MAX_CANDIDATE_CHARS: usize = 128;

/// A successful match: its score and the matched char indices (ascending) into the
/// candidate's `chars()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    pub positions: Vec<usize>,
}

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn bonus(prev: char, cur: char) -> i32 {
    match prev {
        '/' => SCORE_MATCH_SLASH,
        '-' | '_' | ' ' => SCORE_MATCH_WORD,
        '.' => SCORE_MATCH_DOT,
        p if p.is_lowercase() && cur.is_uppercase() => SCORE_MATCH_CAPITAL,
        _ => 0,
    }
}

/// True when `needle` is a case-insensitive subsequence of `haystack`.
pub fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut h = haystack.chars().map(lower);
    needle.chars().map(lower).all(|n| h.any(|c| c == n))
}

/// Matches `query` against `candidate`. An empty query matches everything with score 0
/// and no positions.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<Match> {
    let needle: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(lower)
        .collect();
    if needle.is_empty() {
        return Some(Match {
            score: 0,
            positions: Vec::new(),
        });
    }
    let hay: Vec<char> = candidate.chars().take(MAX_CANDIDATE_CHARS).collect();
    let hay_lower: Vec<char> = hay.iter().map(|c| lower(*c)).collect();
    let (n, m) = (needle.len(), hay.len());
    if n > m || !is_subsequence_chars(&needle, &hay_lower) {
        return None;
    }
    let bonuses: Vec<i32> = (0..m)
        .map(|j| bonus(if j == 0 { '/' } else { hay[j - 1] }, hay[j]))
        .collect();

    // d[i][j]: best score with needle[i] matched exactly at hay[j].
    // best[i][j]: best score for needle[..=i] within hay[..=j].
    let mut d = vec![vec![NEG; m]; n];
    let mut best = vec![vec![NEG; m]; n];
    for i in 0..n {
        let gap = if i == n - 1 {
            SCORE_GAP_TRAILING
        } else {
            SCORE_GAP_INNER
        };
        let mut prev = NEG;
        for j in 0..m {
            if needle[i] == hay_lower[j] {
                let score = if i == 0 {
                    j as i32 * SCORE_GAP_LEADING + bonuses[j]
                } else if j > 0 {
                    let via_gap = best[i - 1][j - 1].saturating_add(bonuses[j]);
                    let via_run = d[i - 1][j - 1].saturating_add(SCORE_MATCH_CONSECUTIVE);
                    via_gap.max(via_run)
                } else {
                    NEG
                };
                d[i][j] = score;
                prev = score.max(prev.saturating_add(gap));
            } else {
                prev = prev.saturating_add(gap);
            }
            best[i][j] = prev;
        }
    }
    let score = best[n - 1][m - 1];
    if score <= NEG / 2 {
        return None;
    }

    // Backtrack the positions.
    let mut positions = vec![0usize; n];
    let mut required = false;
    let mut j = m;
    for i in (0..n).rev() {
        while j > 0 {
            j -= 1;
            let here = d[i][j];
            if here != NEG && (required || here == best[i][j]) {
                required =
                    i > 0 && j > 0 && best[i][j] == d[i - 1][j - 1] + SCORE_MATCH_CONSECUTIVE;
                positions[i] = j;
                break;
            }
        }
    }
    Some(Match { score, positions })
}

fn is_subsequence_chars(needle: &[char], hay: &[char]) -> bool {
    let mut h = hay.iter();
    needle.iter().all(|n| h.any(|c| c == n))
}

/// Splits `text` into `(segment, highlighted)` runs given matched char indices, so a view
/// can draw the matched characters in another color. Runs alternate and cover the text.
pub fn highlight_runs(text: &str, positions: &[usize]) -> Vec<(String, bool)> {
    let mut runs: Vec<(String, bool)> = Vec::new();
    let mut next = positions.iter().copied().peekable();
    for (i, c) in text.chars().enumerate() {
        while next.peek().is_some_and(|p| *p < i) {
            next.next();
        }
        let hit = next.peek() == Some(&i);
        match runs.last_mut() {
            Some((s, h)) if *h == hit => s.push(c),
            _ => runs.push((c.to_string(), hit)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(q: &str, c: &str) -> Option<i32> {
        fuzzy_match(q, c).map(|m| m.score)
    }

    #[test]
    fn empty_query_matches_everything() {
        let m = fuzzy_match("", "anything").unwrap();
        assert_eq!((m.score, m.positions.len()), (0, 0));
        assert!(fuzzy_match("   ", "x").is_some());
    }

    #[test]
    fn requires_an_ordered_subsequence() {
        assert!(fuzzy_match("fx", "Firefox").is_some());
        assert!(fuzzy_match("xf", "Firefox").is_none());
        assert!(fuzzy_match("firefoxx", "Firefox").is_none());
        assert!(fuzzy_match("a", "").is_none());
        assert!(is_subsequence("FF", "firefox"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(score("FIRE", "firefox"), score("fire", "FireFox"));
        assert!(score("fire", "FIREFOX").is_some());
    }

    #[test]
    fn prefix_beats_scattered() {
        let prefix = score("fire", "Firefox").unwrap();
        let scattered = score("fire", "Fantasy Interface Reader Editor").unwrap();
        assert!(prefix > scattered, "{prefix} vs {scattered}");
    }

    #[test]
    fn word_starts_beat_mid_word() {
        let word = score("ff", "Fast Fox").unwrap();
        let mid = score("ff", "Baffle").unwrap();
        assert!(word > mid, "{word} vs {mid}");
        let camel = score("vc", "VisualCode").unwrap();
        let flat = score("vc", "vivacious").unwrap();
        assert!(camel > flat);
    }

    #[test]
    fn consecutive_beats_gapped() {
        assert!(score("abc", "xabcx").unwrap() > score("abc", "xaxbxcx").unwrap());
    }

    #[test]
    fn shorter_candidate_wins_on_equal_match() {
        assert!(
            score("term", "Terminal").unwrap() > score("term", "Terminal Emulator Pro").unwrap()
        );
    }

    #[test]
    fn positions_are_ascending_and_point_at_matches() {
        let c = "Mozilla Firefox";
        let m = fuzzy_match("mf", c).unwrap();
        assert_eq!(m.positions, vec![0, 8]);
        let m = fuzzy_match("fox", c).unwrap();
        assert_eq!(m.positions, vec![12, 13, 14]);
        for q in ["mzl", "ffx", "fire", "moz fir"] {
            let m = fuzzy_match(q, c).unwrap();
            let chars: Vec<char> = c.chars().collect();
            let q: Vec<char> = q.chars().filter(|c| !c.is_whitespace()).collect();
            assert_eq!(m.positions.len(), q.len());
            assert!(m.positions.windows(2).all(|w| w[0] < w[1]));
            for (p, qc) in m.positions.iter().zip(&q) {
                assert_eq!(lower(chars[*p]), lower(*qc));
            }
        }
    }

    #[test]
    fn positions_prefer_the_word_start_over_an_earlier_letter() {
        // "bar" could match the b in "abc" first, but the word start of "bar" scores higher.
        let m = fuzzy_match("b", "abc bar").unwrap();
        assert_eq!(m.positions, vec![4]);
    }

    #[test]
    fn unicode_positions_are_char_indices() {
        let m = fuzzy_match("ö", "Kölner Öl").unwrap();
        assert_eq!(m.positions.len(), 1);
        assert!(m.positions[0] == 1 || m.positions[0] == 7);
    }

    #[test]
    fn overlong_candidates_are_truncated_not_rejected() {
        let long = "a".repeat(500);
        assert!(fuzzy_match("aa", &long).is_some());
        assert!(fuzzy_match("b", &(long + "b")).is_none());
    }

    #[test]
    fn highlight_runs_alternate_and_cover_the_text() {
        let runs = highlight_runs("Firefox", &[0, 1, 4]);
        assert_eq!(
            runs,
            vec![
                ("Fi".to_string(), true),
                ("re".to_string(), false),
                ("f".to_string(), true),
                ("ox".to_string(), false),
            ]
        );
        let joined: String = runs.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(joined, "Firefox");
        assert_eq!(highlight_runs("abc", &[]), vec![("abc".to_string(), false)]);
        assert!(highlight_runs("", &[]).is_empty());
    }
}
