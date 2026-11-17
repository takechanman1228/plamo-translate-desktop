use std::{ops::Range, sync::LazyLock};

use regex::Regex;

const DIGITS: &str = r"[0-9０-９]+(?:[,，][0-9０-９]{3})*(?:[.．][0-9０-９]+)?";
const EN_SCALES: &str = r"trillions?|billions?|millions?|thousands?";
const JP_SCALES: &str = "兆億万千百十";
const DECIMALS: u128 = 1_000_000;

static NUMERIC_EXPRESSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?:{DIGITS}\s+(?:{EN_SCALES})(?:[, ]+|,?\s+and\s+))+{DIGITS}(?:\s+(?:{EN_SCALES}))?\s*(?:yen|JPY)\b|(?:[¥￥]|\bJPY\s*)?{DIGITS}(?:[{JP_SCALES}](?:{DIGITS})?)*(?:円[%％]?|[%％]|\s+(?:(?:{EN_SCALES})\s+)?(?:yen|JPY)\b)?"
    )).unwrap()
});

static JAPANESE_AMOUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?:{DIGITS}[{JP_SCALES}])*(?:{DIGITS})?円")).unwrap());

static JAPANESE_TERM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"({DIGITS})([{JP_SCALES}]?)")).unwrap());

static ENGLISH_AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?P<prefix>\bJPY\s*|[¥￥]\s*)?(?P<value>(?:{DIGITS}\s+(?:{EN_SCALES})(?:,?\s+and\s+|[, ]+))*{DIGITS}(?:\s+(?:{EN_SCALES}))?)(?P<suffix>\s*(?:yen|JPY)\b)?"
    )).unwrap()
});

static ENGLISH_TERM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?i)({DIGITS})(?:\s+({EN_SCALES}))?")).unwrap());

pub(super) fn expressions(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    NUMERIC_EXPRESSION.find_iter(text).map(|m| m.range())
}

fn decimal(text: &str) -> Option<u128> {
    let normalized: String = text
        .chars()
        .filter_map(|c| match c {
            '０'..='９' => char::from_u32(u32::from(c) - u32::from('０') + u32::from('0')),
            '．' => Some('.'),
            ',' | '，' => None,
            c => Some(c),
        })
        .collect();
    let (whole, fraction) = normalized.split_once('.').unwrap_or((&normalized, ""));
    let whole = whole.parse::<u128>().ok()?.checked_mul(DECIMALS)?;
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        return Some(whole);
    }
    let zeros = 6usize.checked_sub(fraction.len())?;
    whole.checked_add(fraction.parse::<u128>().ok()? * 10u128.pow(zeros as u32))
}

fn japanese(text: &str) -> Vec<(u128, bool)> {
    JAPANESE_AMOUNT
        .find_iter(text)
        .filter_map(|m| {
            let expression = m.as_str();
            let mut total = 0u128;
            let mut subtotal = 0u128;
            let mut any_digits = false;
            let mut consumed = 0;
            for term in JAPANESE_TERM.captures_iter(expression) {
                let matched = term.get(0)?;
                if matched.start() != consumed {
                    return None;
                }
                any_digits = true;
                consumed = matched.end();
                let amount = decimal(&term[1])?;
                match &term[2] {
                    "兆" | "億" | "万" => {
                        let factor = match &term[2] {
                            "兆" => 1_000_000_000_000,
                            "億" => 100_000_000,
                            _ => 10_000,
                        };
                        total = total
                            .checked_add(subtotal.checked_add(amount)?.checked_mul(factor)?)?;
                        subtotal = 0;
                    }
                    unit => {
                        let factor = match unit {
                            "千" => 1000,
                            "百" => 100,
                            "十" => 10,
                            _ => 1,
                        };
                        subtotal = subtotal.checked_add(amount.checked_mul(factor)?)?;
                    }
                }
            }
            (any_digits && expression.get(consumed..) == Some("円"))
                .then(|| {
                    total
                        .checked_add(subtotal)
                        .map(|value| (value, expression.contains(['兆', '億', '万'])))
                })
                .flatten()
        })
        .collect()
}

static RANGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:to|and|or|through|[-–—~〜～])\s*$").unwrap());

static ENGLISH_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\byen\b|\bJPY\b|[¥￥]").unwrap());

/// Yen amounts and whether a currency marker remains whose amount could not be read
/// ("one million yen", "¥1M").
fn english(text: &str) -> (Vec<(u128, bool)>, bool) {
    struct Found {
        range: Range<usize>,
        marked: bool,
        amount: Option<(u128, bool)>,
    }
    let mut found: Vec<Found> = ENGLISH_AMOUNT
        .captures_iter(text)
        .map(|amount| {
            let range = amount.get(0).expect("whole match").range();
            let mut scaled = false;
            let mut total = Some(0u128);
            for term in ENGLISH_TERM.captures_iter(&amount["value"]) {
                let scale = term.get(2);
                scaled |= scale.is_some();
                let factor = match scale.map(|m| m.as_str().to_ascii_lowercase()).as_deref() {
                    Some("trillion" | "trillions") => 1_000_000_000_000,
                    Some("billion" | "billions") => 1_000_000_000,
                    Some("million" | "millions") => 1_000_000,
                    Some("thousand" | "thousands") => 1_000,
                    _ => 1,
                };
                total = total
                    .and_then(|total| total.checked_add(decimal(&term[1])?.checked_mul(factor)?));
            }
            // "1M" or "5k": an abbreviation this parser does not read.
            let abbreviated = amount.name("suffix").is_none()
                && text[range.end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic());
            Found {
                marked: amount.name("prefix").is_some() || amount.name("suffix").is_some(),
                amount: total.filter(|_| !abbreviated).map(|total| (total, scaled)),
                range,
            }
        })
        .collect();
    // A range states its currency once: "30,000 to 50,000 yen", "¥30,000–50,000".
    for i in 1..found.len() {
        let joined = RANGE.is_match(&text[found[i - 1].range.end..found[i].range.start]);
        if joined && found[i - 1].marked && !found[i].marked {
            found[i].marked = true;
        }
    }
    for i in (1..found.len()).rev() {
        let joined = RANGE.is_match(&text[found[i - 1].range.end..found[i].range.start]);
        if joined && found[i].marked && !found[i - 1].marked {
            found[i - 1].marked = true;
        }
    }
    let amounts: Vec<_> = found
        .iter()
        .filter(|found| found.marked)
        .filter_map(|found| found.amount)
        .collect();
    let unread = ENGLISH_MARKER.find_iter(text).count() > amounts.len();
    (amounts, unread)
}

/// Explicit scaled yen amounts must survive a Japanese/English translation as exact values.
/// A translation whose amounts cannot be read at all (spelled-out or abbreviated numbers) is
/// left to the reviewer rather than rejected.
pub(crate) fn matches(source: &str, target: &str, language: &str) -> bool {
    let (source, target, unread) = match language {
        "en" => {
            let (target, unread) = english(target);
            (japanese(source), target, unread)
        }
        "ja" => {
            let target_amounts = japanese(target);
            let unread = target.matches('円').count() > target_amounts.len();
            (english(source).0, target_amounts, unread)
        }
        _ => return true,
    };
    if !source.iter().any(|&(_, scaled)| scaled) {
        return true;
    }
    let sorted_values = |amounts: Vec<(u128, bool)>| {
        let mut values: Vec<_> = amounts.into_iter().map(|(value, _)| value).collect();
        values.sort_unstable();
        values
    };
    sorted_values(source) == sorted_values(target) || unread
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn ranges_share_one_currency_marker_and_unread_amounts_are_not_rejected() {
        let range = "価格は3万円から5万円です。";
        assert!(matches(range, "The price is 30,000 to 50,000 yen.", "en"));
        assert!(matches(range, "The price is ¥30,000–50,000.", "en"));
        assert!(!matches(range, "The price is 30,000 to 500,000 yen.", "en"));
        assert!(matches("30,000 to 50,000 yen", "3万円から5万円", "ja"));
        let budget = "予算は100万円です。";
        assert!(matches(budget, "The budget is one million yen.", "en"));
        assert!(matches(budget, "The budget is ¥1M.", "en"));
        assert!(!matches(budget, "The budget is 100 million yen.", "en"));
        assert!(!matches(budget, "The budget is 1 million.", "en"));
    }
}
