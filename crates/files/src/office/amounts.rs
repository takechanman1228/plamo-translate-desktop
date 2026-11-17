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

fn english(text: &str) -> Vec<(u128, bool)> {
    ENGLISH_AMOUNT
        .captures_iter(text)
        .filter_map(|amount| {
            if amount.name("prefix").is_none() && amount.name("suffix").is_none() {
                return None;
            }
            let mut scaled = false;
            let mut total = 0u128;
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
                total = total.checked_add(decimal(&term[1])?.checked_mul(factor)?)?;
            }
            Some((total, scaled))
        })
        .collect()
}

/// Explicit scaled yen amounts must survive a Japanese/English translation as exact values.
pub(crate) fn matches(source: &str, target: &str, language: &str) -> bool {
    let (source, target) = match language {
        "en" => (japanese(source), english(target)),
        "ja" => (english(source), japanese(target)),
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
    sorted_values(source) == sorted_values(target)
}
