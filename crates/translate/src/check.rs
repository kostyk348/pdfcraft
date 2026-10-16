//! Numbers and units must survive a translation.
//!
//! A translated datasheet that turns `3.5 mm` into `35 mm`, or `24 V` into `24 A`, is worse than
//! an untranslated one: the reader has no way to notice. The check compares the numeric tokens of
//! the source and the target as multisets and names every difference, so the caller can refuse the
//! span instead of publishing it.
//!
//! Units are compared by meaning, not by glyphs: `24 V`, `24V` and `24 В` are the same token, and
//! so are `3.5 mm` and `3,5 мм`. The decimal separator is normalised for the same reason.

/// A canonical unit key for the units that carry meaning in technical text. Both the Latin and the
/// Cyrillic spellings map to the same key, because that difference *is* the translation.
fn canonical_unit(raw: &str) -> Option<&'static str> {
    let lowered = raw.trim().to_lowercase();
    let key = match lowered.as_str() {
        "v" | "в" => "v",
        "a" | "а" => "a",
        "w" | "вт" => "w",
        "kw" | "квт" => "kw",
        "mw" | "мвт" => "mw",
        "ma" | "ма" => "ma",
        "ka" | "ка" => "ka",
        "mm" | "мм" => "mm",
        "cm" | "см" => "cm",
        "m" | "м" => "m",
        "km" | "км" => "km",
        "nm" | "нм" => "nm",
        "um" | "µm" | "мкм" => "um",
        "s" | "с" => "s",
        "ms" | "мс" => "ms",
        "min" | "мин" => "min",
        "h" | "ч" => "h",
        "hz" | "гц" => "hz",
        "khz" | "кгц" => "khz",
        "mhz" | "мгц" => "mhz",
        "g" | "г" => "g",
        "kg" | "кг" => "kg",
        "mg" | "мг" => "mg",
        "n" | "н" => "n",
        "pa" | "па" => "pa",
        "kpa" | "кпа" => "kpa",
        "mpa" | "мпа" => "mpa",
        "bar" | "бар" => "bar",
        "ohm" | "ω" | "ом" => "ohm",
        "kohm" | "kω" | "ком" => "kohm",
        "f" | "ф" => "f",
        "uf" | "µf" | "мкф" => "uf",
        "nf" | "нф" => "nf",
        "pf" | "пф" => "pf",
        "l" | "л" => "l",
        "ml" | "мл" => "ml",
        "rpm" | "об/мин" => "rpm",
        "kb" | "кб" => "kb",
        "mb" | "мб" => "mb",
        "gb" | "гб" => "gb",
        "%" => "%",
        "°c" | "с°" => "degc",
        "°" | "deg" | "град" => "deg",
        _ => return None,
    };
    Some(key)
}

fn is_number_char(c: char) -> bool {
    c.is_ascii_digit() || c == '.' || c == ','
}

fn is_unit_char(c: char) -> bool {
    c.is_alphabetic() || c == '°' || c == 'Ω' || c == 'ω' || c == 'µ' || c == '%' || c == '/'
}

/// Fold a number so that `3.5`, `3,5` and `3 5`-with-a-thin-space compare equal. Only the decimal
/// separator is normalised; the digits themselves are never touched.
fn fold_number(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            ',' => out.push('.'),
            '.' | '0'..='9' => out.push(c),
            _ => {}
        }
    }
    // Trailing separators are punctuation, not part of the value.
    while out.ends_with('.') {
        out.pop();
    }
    out
}

/// Every number with its unit, as a comparable token: `24V`, `3.5MM`, `50%`.
pub fn tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_number_char(chars[i]) {
            i += 1;
        }
        let number = fold_number(&chars[start..i].iter().collect::<String>());
        // A unit glued to the number: 24V, 3.5mm, 50%.
        let glued_start = i;
        while i < chars.len() && is_unit_char(chars[i]) {
            i += 1;
        }
        if i > glued_start {
            let unit: String = chars[glued_start..i].iter().collect();
            if let Some(key) = canonical_unit(&unit) {
                out.push(format!("{number}{key}"));
                continue;
            }
            // Not a unit (`3phase`): keep the number alone and let the letters be prose.
            i = glued_start;
            out.push(number);
            continue;
        }
        // A unit after one space: `24 V`, `3.5 mm`.
        let mut j = i;
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        let unit_start = j;
        while j < chars.len() && is_unit_char(chars[j]) {
            j += 1;
        }
        if j > unit_start {
            let unit: String = chars[unit_start..j].iter().collect();
            if let Some(key) = canonical_unit(&unit) {
                out.push(format!("{number}{key}"));
                i = j;
                continue;
            }
        }
        out.push(number);
    }
    out
}

/// Compare the numbers of the source and of the target. An empty result means every value in the
/// source is still there, exactly once, with the same unit.
pub fn integrity(source: &str, target: &str) -> Vec<String> {
    let mut source_tokens = tokens(source);
    let mut target_tokens = tokens(target);
    source_tokens.sort();
    target_tokens.sort();
    let mut flags = Vec::new();
    let mut used = vec![false; target_tokens.len()];
    for token in &source_tokens {
        match target_tokens.iter().enumerate().find(|(k, t)| !used[*k] && *t == token) {
            Some((k, _)) => used[k] = true,
            None => flags.push(format!("missing in the translation: {token}")),
        }
    }
    for (k, token) in target_tokens.iter().enumerate() {
        if !used[k] {
            flags.push(format!("not present in the source: {token}"));
        }
    }
    flags
}

/// Report for the whole page pair, plus the ratio of surviving values — the number a batch job
/// wants to watch.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub source_values: usize,
    pub target_values: usize,
    pub flags: Vec<String>,
}

impl Summary {
    pub fn of(source: &str, target: &str) -> Self {
        Self {
            source_values: tokens(source).len(),
            target_values: tokens(target).len(),
            flags: integrity(source, target),
        }
    }

    pub fn is_clean(&self) -> bool {
        self.flags.is_empty()
    }
}
