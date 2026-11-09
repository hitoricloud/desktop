//! cmdk 1.1's `command-score`: fuzzy match score of an item's text against the search (0 = hidden).
use std::collections::HashMap;

const SCORE_CONTINUE_MATCH: f64 = 1.;
const SCORE_SPACE_WORD_JUMP: f64 = 0.9;
const SCORE_NON_SPACE_WORD_JUMP: f64 = 0.8;
const SCORE_CHARACTER_JUMP: f64 = 0.17;
const SCORE_TRANSPOSITION: f64 = 0.1;
const PENALTY_SKIPPED: f64 = 0.999;
const PENALTY_CASE_MISMATCH: f64 = 0.9999;
const PENALTY_NOT_COMPLETE: f64 = 0.99;

fn is_gap(c: char) -> bool {
    matches!(c, '\\' | '/' | '_' | '+' | '.' | '#' | '"' | '@' | '[' | '(' | '{' | '&')
}

fn is_space(c: char) -> bool {
    c.is_whitespace() || c == '-'
}

/// `string.toLowerCase().replace(/[\\s-]/g, " ")`
fn normalize(s: &[char]) -> Vec<char> {
    s.iter().map(|c| if is_space(*c) { ' ' } else { c.to_lowercase().next().unwrap_or(*c) }).collect()
}

struct Ctx<'a> {
    string: &'a [char],
    abbr: &'a [char],
    lower_string: &'a [char],
    lower_abbr: &'a [char],
    memo: HashMap<(usize, usize), f64>,
}

fn index_of(hay: &[char], c: char, from: usize) -> Option<usize> {
    hay.iter().skip(from).position(|x| *x == c).map(|i| i + from)
}

fn score(cx: &mut Ctx, si: usize, ai: usize) -> f64 {
    if ai == cx.abbr.len() {
        return if si == cx.string.len() { SCORE_CONTINUE_MATCH } else { PENALTY_NOT_COMPLETE };
    }
    if let Some(v) = cx.memo.get(&(si, ai)) {
        return *v;
    }
    let ch = cx.lower_abbr[ai];
    let mut index = index_of(cx.lower_string, ch, si);
    let mut high = 0f64;
    while let Some(i) = index {
        let mut s = score(cx, i + 1, ai + 1);
        if s > high {
            if i == si {
                s *= SCORE_CONTINUE_MATCH;
            } else if i > 0 && is_gap(cx.string[i - 1]) {
                s *= SCORE_NON_SPACE_WORD_JUMP;
                let gaps = cx.string[si..i.saturating_sub(1).max(si)].iter().filter(|c| is_gap(**c)).count();
                if gaps > 0 && si > 0 {
                    s *= PENALTY_SKIPPED.powi(gaps as i32);
                }
            } else if i > 0 && is_space(cx.string[i - 1]) {
                s *= SCORE_SPACE_WORD_JUMP;
                let spaces = cx.string[si..i.saturating_sub(1).max(si)].iter().filter(|c| is_space(**c)).count();
                if spaces > 0 && si > 0 {
                    s *= PENALTY_SKIPPED.powi(spaces as i32);
                }
            } else {
                s *= SCORE_CHARACTER_JUMP;
                if si > 0 {
                    s *= PENALTY_SKIPPED.powi((i - si) as i32);
                }
            }
            if cx.string[i] != cx.abbr[ai] {
                s *= PENALTY_CASE_MISMATCH;
            }
        }
        let next = cx.lower_abbr.get(ai + 1).copied();
        let prev = if i > 0 { cx.lower_string.get(i - 1).copied() } else { None };
        // JS `charAt` past either end is "", so two missing characters compare equal.
        if ai + 1 < cx.abbr.len() && ((s < SCORE_TRANSPOSITION && prev == next) || (next == Some(ch) && prev != Some(ch))) {
            let t = score(cx, i + 1, ai + 2);
            if t * SCORE_TRANSPOSITION > s {
                s = t * SCORE_TRANSPOSITION;
            }
        }
        if s > high {
            high = s;
        }
        index = index_of(cx.lower_string, ch, i + 1);
    }
    cx.memo.insert((si, ai), high);
    high
}

/// `commandScore(value + " " + keywords.join(" "), search)`.
pub fn command_score(value: &str, search: &str) -> f64 {
    let string: Vec<char> = value.chars().collect();
    let abbr: Vec<char> = search.chars().collect();
    let (ls, la) = (normalize(&string), normalize(&abbr));
    let mut cx = Ctx { string: &string, abbr: &abbr, lower_string: &ls, lower_abbr: &la, memo: HashMap::new() };
    score(&mut cx, 0, 0)
}
