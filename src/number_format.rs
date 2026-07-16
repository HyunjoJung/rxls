//! Bounded, dependency-free spreadsheet display-number formatting.
//!
//! This module deliberately produces cell-content text rather than attempting
//! width-dependent layout.  In particular, `_x` reserves one deterministic
//! ASCII space and `*x` consumes the fill directive without repeating it; a
//! renderer can apply cell-width-aware fill after layout if desired.

const MAX_FORMAT_BYTES: usize = 4_096;
const MAX_FORMAT_CHARS: usize = 2_048;
const MAX_ATOMS: usize = 1_024;
const MAX_OUTPUT_BYTES: usize = 16_384;
const MAX_DECIMALS: usize = 30;
const MAX_FRACTION_DENOMINATOR: u64 = 9_999;

#[derive(Clone, Debug)]
enum Atom {
    Char(char),
    Literal(String),
    At,
    Elapsed(char, usize),
}

#[derive(Clone, Copy, Debug)]
enum Comparison {
    Lt,
    Le,
    Eq,
    Ne,
    Ge,
    Gt,
}

#[derive(Clone, Copy, Debug)]
struct Condition {
    comparison: Comparison,
    threshold: f64,
}

impl Condition {
    fn matches(self, value: f64) -> bool {
        match self.comparison {
            Comparison::Lt => value < self.threshold,
            Comparison::Le => value <= self.threshold,
            Comparison::Eq => value == self.threshold,
            Comparison::Ne => value != self.threshold,
            Comparison::Ge => value >= self.threshold,
            Comparison::Gt => value > self.threshold,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
enum Locale {
    #[default]
    English,
    Korean,
    Japanese,
    Chinese,
}

#[derive(Debug)]
struct Section<'a> {
    raw: &'a str,
    atoms: Vec<Atom>,
    condition: Option<Condition>,
    locale: Locale,
}

/// Apply a custom numeric format.  `None` means the format was malformed,
/// over-budget, or could not be represented safely; callers use their stable
/// unformatted fallback in that case.
pub(super) fn render_number(value: f64, code: &str, date1904: bool) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    let sections = parse(code)?;
    let (section, magnitude, automatic_minus) = select_numeric(&sections, value)?;
    let kind = super::classify_string(section.raw);
    let result = if kind.is_datetime() {
        render_datetime(section, value, date1904)?
    } else {
        render_numeric(section, magnitude, automatic_minus)?
    };
    bounded(result)
}

/// Apply a text section (`@`, normally the fourth section) to authored text.
pub(super) fn render_text(text: &str, code: &str) -> Option<String> {
    let sections = parse(code)?;
    let Some(section) = sections.get(3) else {
        return Some(text.to_string());
    };
    let mut out = String::new();
    for atom in &section.atoms {
        match atom {
            Atom::At => out.push_str(text),
            Atom::Literal(s) => out.push_str(s),
            Atom::Char(c) => out.push(*c),
            Atom::Elapsed(c, count) => {
                out.extend(std::iter::repeat_n(*c, *count));
            }
        }
        if out.len() > MAX_OUTPUT_BYTES {
            return None;
        }
    }
    Some(out)
}

fn bounded(value: String) -> Option<String> {
    (value.len() <= MAX_OUTPUT_BYTES).then_some(value)
}

fn parse(code: &str) -> Option<Vec<Section<'_>>> {
    if code.len() > MAX_FORMAT_BYTES || code.chars().count() > MAX_FORMAT_CHARS {
        return None;
    }
    let ranges = split_sections(code)?;
    let mut sections = Vec::with_capacity(ranges.len());
    for raw in ranges {
        sections.push(parse_section(raw)?);
    }
    Some(sections)
}

fn split_sections(code: &str) -> Option<Vec<&str>> {
    let mut sections = Vec::new();
    let mut start = 0;
    let mut chars = code.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => {
                let mut closed = false;
                for (_, next) in chars.by_ref() {
                    if next == '"' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return None;
                }
            }
            '[' => {
                let mut closed = false;
                for (_, next) in chars.by_ref() {
                    if next == ']' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return None;
                }
            }
            '\\' | '_' | '*' => {
                chars.next()?;
            }
            ';' => {
                if sections.len() == 3 {
                    return None;
                }
                sections.push(&code[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    sections.push(&code[start..]);
    Some(sections)
}

fn parse_section(raw: &str) -> Option<Section<'_>> {
    let chars: Vec<char> = raw.chars().collect();
    let mut atoms = Vec::new();
    let mut condition = None;
    let mut locale = Locale::English;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
                if i == chars.len() {
                    return None;
                }
                push_literal(&mut atoms, chars[start..i].iter().collect());
                i += 1;
            }
            '\\' => {
                i += 1;
                let escaped = *chars.get(i)?;
                push_literal(&mut atoms, escaped.to_string());
                i += 1;
            }
            '_' => {
                i += 1;
                chars.get(i)?;
                push_literal(&mut atoms, " ".to_string());
                i += 1;
            }
            '*' => {
                // Fill is cell-width dependent. Consume its operand so it can
                // never be mistaken for a number/date token.
                i += 1;
                chars.get(i)?;
                i += 1;
            }
            '[' => {
                let start = i + 1;
                i = start;
                while i < chars.len() && chars[i] != ']' {
                    i += 1;
                }
                if i == chars.len() {
                    return None;
                }
                let inner: String = chars[start..i].iter().collect();
                i += 1;
                if let Some(parsed) = parse_condition(&inner) {
                    if condition.replace(parsed).is_some() {
                        return None;
                    }
                } else if inner
                    .chars()
                    .next()
                    .is_some_and(|ch| matches!(ch, '<' | '>' | '='))
                {
                    return None;
                } else if let Some((field, count)) = parse_elapsed(&inner) {
                    atoms.push(Atom::Elapsed(field, count));
                } else if inner.starts_with('$') {
                    let (currency, parsed_locale) = parse_locale(&inner);
                    locale = parsed_locale.unwrap_or(locale);
                    if !currency.is_empty() {
                        push_literal(&mut atoms, currency);
                    }
                } else if !is_color(&inner) && !is_ignored_directive(&inner) {
                    push_literal(&mut atoms, format!("[{inner}]"));
                }
            }
            '@' => {
                atoms.push(Atom::At);
                i += 1;
            }
            ch => {
                atoms.push(Atom::Char(ch));
                i += 1;
            }
        }
        if atoms.len() > MAX_ATOMS {
            return None;
        }
    }
    Some(Section {
        raw,
        atoms,
        condition,
        locale,
    })
}

fn push_literal(atoms: &mut Vec<Atom>, value: String) {
    if value.is_empty() {
        return;
    }
    if let Some(Atom::Literal(existing)) = atoms.last_mut() {
        existing.push_str(&value);
    } else {
        atoms.push(Atom::Literal(value));
    }
}

fn parse_condition(inner: &str) -> Option<Condition> {
    let (comparison, rest) = if let Some(rest) = inner.strip_prefix("<=") {
        (Comparison::Le, rest)
    } else if let Some(rest) = inner.strip_prefix(">=") {
        (Comparison::Ge, rest)
    } else if let Some(rest) = inner.strip_prefix("<>") {
        (Comparison::Ne, rest)
    } else if let Some(rest) = inner.strip_prefix('<') {
        (Comparison::Lt, rest)
    } else if let Some(rest) = inner.strip_prefix('>') {
        (Comparison::Gt, rest)
    } else if let Some(rest) = inner.strip_prefix('=') {
        (Comparison::Eq, rest)
    } else {
        return None;
    };
    let threshold = rest.trim().parse::<f64>().ok()?;
    threshold.is_finite().then_some(Condition {
        comparison,
        threshold,
    })
}

fn parse_elapsed(inner: &str) -> Option<(char, usize)> {
    let mut chars = inner.chars();
    let first = chars.next()?.to_ascii_lowercase();
    if !matches!(first, 'h' | 'm' | 's') {
        return None;
    }
    let mut count = 1;
    for ch in chars {
        if ch.to_ascii_lowercase() != first {
            return None;
        }
        count += 1;
    }
    Some((first, count))
}

fn parse_locale(inner: &str) -> (String, Option<Locale>) {
    let rest = inner.strip_prefix('$').unwrap_or(inner);
    let (currency, locale_id) = match rest.rsplit_once('-') {
        Some((currency, id)) => (currency, Some(id)),
        None => (rest, None),
    };
    let locale = locale_id
        .and_then(|id| u32::from_str_radix(id, 16).ok())
        .and_then(|id| match id & 0xffff {
            0x0412 => Some(Locale::Korean),
            0x0411 => Some(Locale::Japanese),
            0x0804 | 0x0404 | 0x0c04 | 0x1004 => Some(Locale::Chinese),
            0x0409 => Some(Locale::English),
            _ => None,
        });
    (currency.to_string(), locale)
}

fn is_color(inner: &str) -> bool {
    let lower = inner.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "black" | "blue" | "cyan" | "green" | "magenta" | "red" | "white" | "yellow"
    ) || lower
        .strip_prefix("color")
        .is_some_and(|number| number.parse::<u8>().is_ok())
}

fn is_ignored_directive(inner: &str) -> bool {
    let lower = inner.to_ascii_lowercase();
    lower.starts_with("dbnum") || lower.starts_with("natnum")
}

fn select_numeric<'a>(
    sections: &'a [Section<'a>],
    value: f64,
) -> Option<(&'a Section<'a>, f64, bool)> {
    let numeric_len = sections.len().min(3);
    if numeric_len == 0 {
        return None;
    }
    let numeric = &sections[..numeric_len];
    if numeric.iter().any(|section| section.condition.is_some()) {
        for section in numeric {
            if section
                .condition
                .is_some_and(|condition| condition.matches(value))
            {
                return Some((section, value.abs(), value.is_sign_negative()));
            }
        }
        // Excel permits one unconditional fallback after conditional sections.
        let fallback = numeric
            .iter()
            .rev()
            .find(|section| section.condition.is_none())?;
        return Some((fallback, value.abs(), value.is_sign_negative()));
    }
    match numeric.len() {
        1 => Some((&numeric[0], value.abs(), value.is_sign_negative())),
        2 if value.is_sign_negative() => Some((&numeric[1], value.abs(), false)),
        2 => Some((&numeric[0], value, false)),
        _ if value > 0.0 => Some((&numeric[0], value, false)),
        _ if value < 0.0 => Some((&numeric[1], value.abs(), false)),
        _ => Some((&numeric[2], 0.0, false)),
    }
}

fn render_numeric(section: &Section<'_>, value: f64, automatic_minus: bool) -> Option<String> {
    if contains_general(&section.atoms) {
        return render_general(&section.atoms, value, automatic_minus);
    }
    if scientific_index(&section.atoms).is_some() {
        return render_scientific(&section.atoms, value, automatic_minus);
    }
    if fraction_index(&section.atoms).is_some() {
        return render_fraction(&section.atoms, value, automatic_minus);
    }
    render_fixed(&section.atoms, value, automatic_minus)
}

fn is_placeholder(atom: &Atom) -> bool {
    matches!(atom, Atom::Char('0' | '#' | '?'))
}

fn placeholder_char(atom: &Atom) -> Option<char> {
    match atom {
        Atom::Char(ch @ ('0' | '#' | '?')) => Some(*ch),
        _ => None,
    }
}

fn contains_general(atoms: &[Atom]) -> bool {
    find_ascii_sequence(atoms, 0, "general").is_some()
}

fn scientific_index(atoms: &[Atom]) -> Option<usize> {
    atoms.iter().enumerate().find_map(|(index, atom)| {
        if !matches!(atom, Atom::Char('e' | 'E')) {
            return None;
        }
        let before = atoms[..index].iter().any(is_placeholder);
        let after = atoms[index + 1..].iter().any(is_placeholder);
        (before && after).then_some(index)
    })
}

fn fraction_index(atoms: &[Atom]) -> Option<usize> {
    atoms.iter().enumerate().find_map(|(index, atom)| {
        if !matches!(atom, Atom::Char('/')) {
            return None;
        }
        let before = atoms[..index].iter().rev().any(is_placeholder);
        let after = atoms[index + 1..]
            .iter()
            .any(|atom| is_placeholder(atom) || matches!(atom, Atom::Char('1'..='9')));
        (before && after).then_some(index)
    })
}

fn render_general(atoms: &[Atom], value: f64, automatic_minus: bool) -> Option<String> {
    let start = find_ascii_sequence(atoms, 0, "general")?;
    let end = start + "general".len();
    let mut out = String::new();
    if automatic_minus {
        out.push('-');
    }
    render_atoms_literal(&mut out, &atoms[..start], &[]);
    out.push_str(&crate::format_number(value));
    render_atoms_literal(&mut out, &atoms[end..], &[]);
    Some(out)
}

fn render_fixed(atoms: &[Atom], value: f64, automatic_minus: bool) -> Option<String> {
    let first = atoms.iter().position(is_placeholder);
    let last = atoms.iter().rposition(is_placeholder);
    let (Some(first), Some(last)) = (first, last) else {
        let mut out = String::new();
        render_atoms_literal(&mut out, atoms, &[]);
        return Some(out);
    };

    let percent_count = atoms
        .iter()
        .filter(|atom| matches!(atom, Atom::Char('%')))
        .count()
        .min(6);
    let mut scaled = value * 100_f64.powi(percent_count as i32);

    let mut scaling_commas = Vec::new();
    let mut index = last + 1;
    while matches!(atoms.get(index), Some(Atom::Char(','))) {
        scaling_commas.push(index);
        index += 1;
    }
    scaled /= 1_000_f64.powi(scaling_commas.len() as i32);

    let decimal = atoms[first..=last]
        .iter()
        .position(|atom| matches!(atom, Atom::Char('.')))
        .map(|offset| first + offset);
    let integer_end = decimal.unwrap_or(last + 1);
    let integer_pattern: Vec<char> = atoms[first..integer_end]
        .iter()
        .filter_map(placeholder_char)
        .collect();
    let fraction_pattern: Vec<char> = decimal
        .map(|decimal| {
            atoms[decimal + 1..=last]
                .iter()
                .filter_map(placeholder_char)
                .take(MAX_DECIMALS)
                .collect()
        })
        .unwrap_or_default();
    let grouping = atoms[first..integer_end]
        .iter()
        .any(|atom| matches!(atom, Atom::Char(',')));

    let precision = fraction_pattern.len();
    let rounded = round_to_precision(scaled, precision);
    let rendered = format!("{rounded:.precision$}");
    let (integer_digits, fraction_digits) = rendered.split_once('.').unwrap_or((&rendered, ""));
    let integer = format_integer_layout(
        integer_digits,
        &atoms[first..integer_end],
        &integer_pattern,
        grouping,
    );
    let fraction = decimal
        .map(|decimal| {
            format_fraction_layout(
                fraction_digits,
                &atoms[decimal + 1..=last],
                &fraction_pattern,
            )
        })
        .unwrap_or_default();

    let mut out = String::new();
    if automatic_minus {
        out.push('-');
    }
    render_atoms_literal(&mut out, &atoms[..first], &[]);
    out.push_str(&integer);
    if decimal.is_some() && (!fraction.is_empty() || fraction_pattern.contains(&'0')) {
        out.push('.');
    }
    out.push_str(&fraction);
    render_atoms_literal(&mut out, &atoms[last + 1 + scaling_commas.len()..], &[]);
    Some(out)
}

fn round_to_precision(value: f64, precision: usize) -> f64 {
    if precision > 15 {
        return value;
    }
    let scale = 10_f64.powi(precision as i32);
    let scaled = value * scale;
    if scaled.is_finite() {
        scaled.round() / scale
    } else {
        value
    }
}

fn format_integer(digits: &str, pattern: &[char], grouping: bool) -> String {
    let digits = digits.trim_start_matches('-');
    let required = pattern.iter().filter(|&&ch| ch == '0').count();
    let mut value = if digits == "0" && required == 0 {
        String::new()
    } else {
        digits.to_string()
    };
    if value.len() < required {
        value.insert_str(0, &"0".repeat(required - value.len()));
    }
    let reserve = pattern.iter().filter(|&&ch| ch == '?').count();
    let missing = pattern.len().saturating_sub(value.len());
    if reserve > 0 && missing > 0 {
        value.insert_str(0, &" ".repeat(missing.min(reserve)));
    }
    if grouping {
        group_digits(&value)
    } else {
        value
    }
}

fn format_integer_layout(digits: &str, atoms: &[Atom], pattern: &[char], grouping: bool) -> String {
    let has_embedded_literal = atoms.iter().any(|atom| match atom {
        Atom::Literal(_) | Atom::At | Atom::Elapsed(_, _) => true,
        Atom::Char(ch) => !matches!(ch, '0' | '#' | '?' | ','),
    });
    if !has_embedded_literal {
        return format_integer(digits, pattern, grouping);
    }

    let required = pattern.iter().filter(|&&ch| ch == '0').count();
    let raw = digits.trim_start_matches('-');
    let raw = if raw == "0" && required == 0 { "" } else { raw };
    let digit_chars: Vec<char> = raw.chars().collect();
    let mut cursor = digit_chars.len();
    let mut slots = vec![None; atoms.len()];
    for (index, atom) in atoms.iter().enumerate().rev() {
        let Some(placeholder) = placeholder_char(atom) else {
            continue;
        };
        slots[index] = if cursor > 0 {
            cursor -= 1;
            Some(digit_chars[cursor])
        } else {
            match placeholder {
                '0' => Some('0'),
                '?' => Some(' '),
                _ => None,
            }
        };
    }
    let mut out: String = digit_chars[..cursor].iter().collect();
    for (index, atom) in atoms.iter().enumerate() {
        if is_placeholder(atom) {
            if let Some(ch) = slots[index] {
                out.push(ch);
            }
        } else {
            render_atoms_literal(&mut out, std::slice::from_ref(atom), &[]);
        }
    }
    out
}

fn group_digits(value: &str) -> String {
    let leading_spaces = value.chars().take_while(|ch| *ch == ' ').count();
    let digits = &value[leading_spaces..];
    if digits.len() <= 3 {
        return value.to_string();
    }
    let mut out = " ".repeat(leading_spaces);
    let first = digits.len() % 3;
    let first = if first == 0 { 3 } else { first };
    out.push_str(&digits[..first]);
    let mut index = first;
    while index < digits.len() {
        out.push(',');
        out.push_str(&digits[index..index + 3]);
        index += 3;
    }
    out
}

fn format_fraction_digits(digits: &str, pattern: &[char]) -> String {
    if pattern.is_empty() {
        return String::new();
    }
    let required_end = pattern
        .iter()
        .rposition(|&ch| ch == '0')
        .map_or(0, |index| index + 1);
    let nonzero_end = digits
        .as_bytes()
        .iter()
        .rposition(|&digit| digit != b'0')
        .map_or(0, |index| index + 1);
    let visible_end = required_end.max(nonzero_end);
    let mut out = String::new();
    for (index, placeholder) in pattern.iter().enumerate() {
        if index < visible_end {
            out.push(digits.as_bytes().get(index).copied().unwrap_or(b'0') as char);
        } else if *placeholder == '?' {
            out.push(' ');
        }
    }
    out
}

fn format_fraction_layout(digits: &str, atoms: &[Atom], pattern: &[char]) -> String {
    let has_embedded_literal = atoms.iter().any(|atom| match atom {
        Atom::Literal(_) | Atom::At | Atom::Elapsed(_, _) => true,
        Atom::Char(ch) => !matches!(ch, '0' | '#' | '?'),
    });
    if !has_embedded_literal {
        return format_fraction_digits(digits, pattern);
    }
    let required_end = pattern
        .iter()
        .rposition(|&ch| ch == '0')
        .map_or(0, |index| index + 1);
    let nonzero_end = digits
        .as_bytes()
        .iter()
        .rposition(|&digit| digit != b'0')
        .map_or(0, |index| index + 1);
    let visible_end = required_end.max(nonzero_end);
    let mut ordinal = 0;
    let mut out = String::new();
    for atom in atoms {
        if let Some(placeholder) = placeholder_char(atom) {
            if ordinal < visible_end {
                out.push(digits.as_bytes().get(ordinal).copied().unwrap_or(b'0') as char);
            } else if placeholder == '?' {
                out.push(' ');
            }
            ordinal += 1;
        } else {
            render_atoms_literal(&mut out, std::slice::from_ref(atom), &[]);
        }
    }
    out
}

fn render_scientific(atoms: &[Atom], value: f64, automatic_minus: bool) -> Option<String> {
    let exponent_index = scientific_index(atoms)?;
    let first = atoms[..exponent_index].iter().position(is_placeholder)?;
    let exponent_first = atoms[exponent_index + 1..]
        .iter()
        .position(is_placeholder)
        .map(|offset| exponent_index + 1 + offset)?;
    let exponent_last = atoms[exponent_first..]
        .iter()
        .take_while(|atom| is_placeholder(atom))
        .count()
        .checked_sub(1)
        .map(|offset| exponent_first + offset)?;
    let decimal = atoms[first..exponent_index]
        .iter()
        .position(|atom| matches!(atom, Atom::Char('.')))
        .map(|offset| first + offset);
    let integer_end = decimal.unwrap_or(exponent_index);
    let integer_pattern: Vec<char> = atoms[first..integer_end]
        .iter()
        .filter_map(placeholder_char)
        .collect();
    let fraction_pattern: Vec<char> = decimal
        .map(|decimal| {
            atoms[decimal + 1..exponent_index]
                .iter()
                .filter_map(placeholder_char)
                .take(MAX_DECIMALS)
                .collect()
        })
        .unwrap_or_default();
    let percent_count = atoms
        .iter()
        .filter(|atom| matches!(atom, Atom::Char('%')))
        .count()
        .min(6);
    let scaled = value * 100_f64.powi(percent_count as i32);
    let integer_places = integer_pattern.len().max(1) as i32;
    let mut exponent = if scaled == 0.0 {
        0
    } else {
        scaled.abs().log10().floor() as i32
    };
    exponent -= exponent.rem_euclid(integer_places);
    let mut mantissa = if scaled == 0.0 {
        0.0
    } else {
        scaled / 10_f64.powi(exponent)
    };
    let precision = fraction_pattern.len();
    let threshold = 10_f64.powi(integer_places);
    let rounding_scale = 10_f64.powi(precision as i32);
    let rounded = (mantissa * rounding_scale).round() / rounding_scale;
    if rounded >= threshold {
        mantissa = rounded / threshold;
        exponent += integer_places;
    }
    let mantissa_text = format!("{mantissa:.precision$}");
    let (integer_digits, fraction_digits) = mantissa_text
        .split_once('.')
        .unwrap_or((&mantissa_text, ""));

    let mut out = String::new();
    if automatic_minus {
        out.push('-');
    }
    render_atoms_literal(&mut out, &atoms[..first], &[]);
    out.push_str(&format_integer(integer_digits, &integer_pattern, false));
    if decimal.is_some() && !fraction_pattern.is_empty() {
        out.push('.');
        out.push_str(&format_fraction_digits(fraction_digits, &fraction_pattern));
    }
    if let Atom::Char(marker) = atoms[exponent_index] {
        out.push(marker);
    }
    let explicit_plus = matches!(atoms.get(exponent_index + 1), Some(Atom::Char('+')));
    if exponent < 0 {
        out.push('-');
    } else if explicit_plus {
        out.push('+');
    }
    let exponent_width = atoms[exponent_first..=exponent_last]
        .iter()
        .filter(|atom| is_placeholder(atom))
        .count();
    let exponent_digits = exponent.unsigned_abs().to_string();
    if exponent_digits.len() < exponent_width {
        out.push_str(&"0".repeat(exponent_width - exponent_digits.len()));
    }
    out.push_str(&exponent_digits);
    render_atoms_literal(&mut out, &atoms[exponent_last + 1..], &[]);
    Some(out)
}

fn render_fraction(atoms: &[Atom], value: f64, automatic_minus: bool) -> Option<String> {
    let slash = fraction_index(atoms)?;
    let mut numerator_start = slash;
    while numerator_start > 0 && is_placeholder(&atoms[numerator_start - 1]) {
        numerator_start -= 1;
    }
    if numerator_start == slash {
        return None;
    }
    let mut denominator_end = slash + 1;
    while denominator_end < atoms.len()
        && (is_placeholder(&atoms[denominator_end])
            || matches!(atoms[denominator_end], Atom::Char('0'..='9')))
    {
        denominator_end += 1;
    }
    if denominator_end == slash + 1 {
        return None;
    }
    let first = atoms[..numerator_start]
        .iter()
        .position(is_placeholder)
        .unwrap_or(numerator_start);
    let whole_last = atoms[..numerator_start].iter().rposition(is_placeholder);
    let whole_pattern: Vec<char> = atoms[first..numerator_start]
        .iter()
        .filter_map(placeholder_char)
        .collect();
    let numerator_pattern: Vec<char> = atoms[numerator_start..slash]
        .iter()
        .filter_map(placeholder_char)
        .collect();
    let denominator_pattern: Vec<char> = atoms[slash + 1..denominator_end]
        .iter()
        .filter_map(placeholder_char)
        .collect();
    let has_fixed_denominator = atoms[slash + 1..denominator_end]
        .iter()
        .any(|atom| matches!(atom, Atom::Char('1'..='9')));
    let fixed_denominator: String = if has_fixed_denominator {
        atoms[slash + 1..denominator_end]
            .iter()
            .filter_map(|atom| match atom {
                Atom::Char(ch @ '0'..='9') => Some(*ch),
                _ => None,
            })
            .collect()
    } else {
        String::new()
    };
    let max_denominator = if has_fixed_denominator {
        fixed_denominator.parse::<u64>().ok()?.max(1)
    } else {
        let digits = denominator_pattern.len().min(4);
        (10_u64.pow(digits as u32) - 1).clamp(1, MAX_FRACTION_DENOMINATOR)
    };
    let mixed = !whole_pattern.is_empty();
    let whole = if mixed { value.floor() as u64 } else { 0 };
    let fraction_value = if mixed { value - whole as f64 } else { value };
    let (mut numerator, denominator) = if has_fixed_denominator {
        (
            (fraction_value * max_denominator as f64).round() as u64,
            max_denominator,
        )
    } else {
        best_fraction(fraction_value, max_denominator)
    };
    let mut whole = whole;
    if mixed && numerator >= denominator {
        whole = whole.saturating_add(numerator / denominator);
        numerator %= denominator;
    }

    let mut out = String::new();
    if automatic_minus {
        out.push('-');
    }
    render_atoms_literal(&mut out, &atoms[..first], &[]);
    if mixed {
        out.push_str(&format_integer(&whole.to_string(), &whole_pattern, false));
    }
    if numerator != 0 || !mixed {
        let separator_start = whole_last.map_or(first, |index| index + 1);
        render_atoms_literal(&mut out, &atoms[separator_start..numerator_start], &[]);
        out.push_str(&format_integer(
            &numerator.to_string(),
            &numerator_pattern,
            false,
        ));
        out.push('/');
        let denominator_display_pattern = if has_fixed_denominator {
            vec!['0'; fixed_denominator.len()]
        } else {
            denominator_pattern
        };
        out.push_str(&format_integer(
            &denominator.to_string(),
            &denominator_display_pattern,
            false,
        ));
    }
    render_atoms_literal(&mut out, &atoms[denominator_end..], &[]);
    Some(out)
}

fn best_fraction(value: f64, max_denominator: u64) -> (u64, u64) {
    let mut best_numerator = 0;
    let mut best_denominator = 1;
    let mut best_error = f64::INFINITY;
    for denominator in 1..=max_denominator {
        let numerator = (value * denominator as f64).round().max(0.0) as u64;
        let error = (value - numerator as f64 / denominator as f64).abs();
        if error < best_error {
            best_error = error;
            best_numerator = numerator;
            best_denominator = denominator;
            if error == 0.0 {
                break;
            }
        }
    }
    let gcd = gcd(best_numerator, best_denominator).max(1);
    (best_numerator / gcd, best_denominator / gcd)
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn render_atoms_literal(out: &mut String, atoms: &[Atom], skip_indexes: &[usize]) {
    for (index, atom) in atoms.iter().enumerate() {
        if skip_indexes.contains(&index) {
            continue;
        }
        match atom {
            Atom::Char(ch) => out.push(*ch),
            Atom::Literal(s) => out.push_str(s),
            Atom::At => out.push('@'),
            Atom::Elapsed(ch, count) => {
                out.push('[');
                out.extend(std::iter::repeat_n(*ch, *count));
                out.push(']');
            }
        }
    }
}

fn render_datetime(section: &Section<'_>, value: f64, date1904: bool) -> Option<String> {
    let fractional_precision = date_fractional_precision(&section.atoms);
    let parts = RoundedDateParts::new(value, date1904, fractional_precision)?;
    let elapsed = section
        .atoms
        .iter()
        .any(|atom| matches!(atom, Atom::Elapsed(_, _)));
    let elapsed_parts = elapsed
        .then(|| ElapsedParts::new(value, fractional_precision))
        .flatten();
    let has_ampm = find_ascii_sequence(&section.atoms, 0, "am/pm").is_some()
        || find_ascii_sequence(&section.atoms, 0, "a/p").is_some();
    let mut out = String::new();
    let mut index = 0;
    let mut previous_was_seconds = false;
    while index < section.atoms.len() {
        if let Some(start) = find_ascii_sequence(&section.atoms, index, "am/pm") {
            if start == index {
                out.push_str(am_pm(parts.h, section.locale, false));
                index += 5;
                previous_was_seconds = false;
                continue;
            }
        }
        if let Some(start) = find_ascii_sequence(&section.atoms, index, "a/p") {
            if start == index {
                out.push_str(am_pm(parts.h, section.locale, true));
                index += 3;
                previous_was_seconds = false;
                continue;
            }
        }
        match &section.atoms[index] {
            Atom::Literal(literal) => {
                out.push_str(literal);
                previous_was_seconds = false;
                index += 1;
            }
            Atom::At => {
                out.push('@');
                previous_was_seconds = false;
                index += 1;
            }
            Atom::Elapsed(field, width) => {
                let elapsed = elapsed_parts?;
                let component = match field {
                    'h' => elapsed.total_hours,
                    'm' => elapsed.total_minutes,
                    's' => elapsed.total_seconds,
                    _ => return None,
                };
                push_padded(&mut out, component, *width);
                previous_was_seconds = *field == 's';
                index += 1;
            }
            Atom::Char('.') if previous_was_seconds => {
                let zeros = section.atoms[index + 1..]
                    .iter()
                    .take_while(|atom| matches!(atom, Atom::Char('0')))
                    .count()
                    .min(9);
                out.push('.');
                if zeros > 0 {
                    let fraction = elapsed_parts
                        .map(|parts| parts.fraction)
                        .unwrap_or(parts.fraction);
                    out.push_str(&format!("{fraction:0zeros$}"));
                    index += zeros;
                }
                previous_was_seconds = false;
                index += 1;
            }
            Atom::Char(ch) if matches!(ch.to_ascii_lowercase(), 'y' | 'm' | 'd' | 'h' | 's') => {
                let field = ch.to_ascii_lowercase();
                let width = run_width(&section.atoms, index, field);
                match field {
                    'y' => format_year(&mut out, parts.y, width),
                    'm' if is_minute_field(&section.atoms, index, width) => {
                        push_padded(&mut out, parts.mi as u64, width)
                    }
                    'm' => format_month(&mut out, parts.mo, width, section.locale),
                    'd' => format_day(&mut out, parts.y, parts.mo, parts.d, width, section.locale),
                    'h' => {
                        let hour = if has_ampm {
                            let hour = parts.h % 12;
                            if hour == 0 {
                                12
                            } else {
                                hour
                            }
                        } else {
                            parts.h
                        };
                        push_padded(&mut out, hour as u64, width);
                    }
                    's' => push_padded(&mut out, parts.s as u64, width),
                    _ => {}
                }
                previous_was_seconds = field == 's';
                index += width;
            }
            Atom::Char(ch) => {
                out.push(*ch);
                // A decimal point directly after seconds retains the signal;
                // other punctuation should not make a later `.0` fractional.
                if *ch != ':' {
                    previous_was_seconds = false;
                }
                index += 1;
            }
        }
        if out.len() > MAX_OUTPUT_BYTES {
            return None;
        }
    }
    Some(out)
}

#[derive(Clone, Copy)]
struct RoundedDateParts {
    y: i64,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
    fraction: u64,
}

impl RoundedDateParts {
    fn new(value: f64, date1904: bool, precision: usize) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        let precision = precision.min(9);
        let scale = 10_u64.checked_pow(precision as u32)?;
        let whole = value.floor();
        let units_per_day = 86_400_u64.checked_mul(scale)?;
        let units = ((value - whole) * units_per_day as f64).round() as u64;
        let day_carry = units / units_per_day;
        let units = units % units_per_day;
        let base = super::serial_to_datetime(whole + day_carry as f64, date1904)?;
        let total_seconds = units / scale;
        Some(Self {
            y: base.y,
            mo: base.mo,
            d: base.d,
            h: (total_seconds / 3_600) as u32,
            mi: ((total_seconds % 3_600) / 60) as u32,
            s: (total_seconds % 60) as u32,
            fraction: units % scale,
        })
    }
}

#[derive(Clone, Copy)]
struct ElapsedParts {
    total_hours: u64,
    total_minutes: u64,
    total_seconds: u64,
    fraction: u64,
}

impl ElapsedParts {
    fn new(value: f64, precision: usize) -> Option<Self> {
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        let scale = 10_u64.checked_pow(precision.min(9) as u32)?;
        let units = value * 86_400.0 * scale as f64;
        if !units.is_finite() || units > u64::MAX as f64 {
            return None;
        }
        let units = units.round() as u64;
        let total_seconds = units / scale;
        Some(Self {
            total_hours: total_seconds / 3_600,
            total_minutes: total_seconds / 60,
            total_seconds,
            fraction: units % scale,
        })
    }
}

fn date_fractional_precision(atoms: &[Atom]) -> usize {
    for index in 0..atoms.len() {
        if matches!(atoms[index], Atom::Char('.')) {
            let zeros = atoms[index + 1..]
                .iter()
                .take_while(|atom| matches!(atom, Atom::Char('0')))
                .count();
            if zeros > 0 {
                return zeros.min(9);
            }
        }
    }
    0
}

fn run_width(atoms: &[Atom], start: usize, field: char) -> usize {
    atoms[start..]
        .iter()
        .take_while(|atom| match atom {
            Atom::Char(ch) => ch.to_ascii_lowercase() == field,
            _ => false,
        })
        .count()
}

fn adjacent_field(atoms: &[Atom], mut index: isize, step: isize) -> Option<char> {
    while index >= 0 && (index as usize) < atoms.len() {
        match &atoms[index as usize] {
            Atom::Char(ch) if matches!(ch.to_ascii_lowercase(), 'y' | 'm' | 'd' | 'h' | 's') => {
                return Some(ch.to_ascii_lowercase());
            }
            Atom::Elapsed(ch, _) => return Some(*ch),
            Atom::Literal(_) | Atom::At => return None,
            Atom::Char(_) => {}
        }
        index += step;
    }
    None
}

fn is_minute_field(atoms: &[Atom], start: usize, width: usize) -> bool {
    let previous = adjacent_field(atoms, start as isize - 1, -1);
    let next = adjacent_field(atoms, (start + width) as isize, 1);
    matches!(previous, Some('h')) || matches!(next, Some('s'))
}

fn format_year(out: &mut String, year: i64, width: usize) {
    if width <= 2 {
        out.push_str(&format!("{:02}", year.rem_euclid(100)));
    } else {
        out.push_str(&format!("{:0width$}", year, width = width.max(4)));
    }
}

fn format_month(out: &mut String, month: u32, width: usize, locale: Locale) {
    if width == 1 {
        out.push_str(&month.to_string());
    } else if width == 2 {
        out.push_str(&format!("{month:02}"));
    } else {
        let name = month_name(month, width >= 4, locale);
        if width >= 5 {
            out.push(name.chars().next().unwrap_or_default());
        } else {
            out.push_str(&name);
        }
    }
}

fn month_name(month: u32, long: bool, locale: Locale) -> String {
    const SHORT: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const LONG: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let index = month.saturating_sub(1).min(11) as usize;
    match locale {
        Locale::Korean | Locale::Japanese | Locale::Chinese => format!("{month}월"),
        Locale::English if long => LONG[index].to_string(),
        Locale::English => SHORT[index].to_string(),
    }
}

fn format_day(out: &mut String, year: i64, month: u32, day: u32, width: usize, locale: Locale) {
    if width == 1 {
        out.push_str(&day.to_string());
    } else if width == 2 {
        out.push_str(&format!("{day:02}"));
    } else {
        let weekday = (super::days_from_civil(year, month, day) + 4).rem_euclid(7) as usize;
        const SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        const LONG: [&str; 7] = [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ];
        const CJK: [&str; 7] = ["일", "월", "화", "수", "목", "금", "토"];
        match locale {
            Locale::Korean => {
                out.push_str(CJK[weekday]);
                if width >= 4 {
                    out.push_str("요일");
                }
            }
            Locale::Japanese | Locale::Chinese => out.push_str(CJK[weekday]),
            Locale::English if width >= 4 => out.push_str(LONG[weekday]),
            Locale::English => out.push_str(SHORT[weekday]),
        }
    }
}

fn am_pm(hour: u32, locale: Locale, short: bool) -> &'static str {
    match (locale, hour < 12, short) {
        (Locale::Korean, true, _) => "오전",
        (Locale::Korean, false, _) => "오후",
        (_, true, true) => "A",
        (_, false, true) => "P",
        (_, true, false) => "AM",
        (_, false, false) => "PM",
    }
}

fn push_padded(out: &mut String, value: u64, width: usize) {
    if width <= 1 {
        out.push_str(&value.to_string());
    } else {
        out.push_str(&format!("{value:0width$}"));
    }
}

fn find_ascii_sequence(atoms: &[Atom], start: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || atoms.len() < needle.len() {
        return None;
    }
    for index in start..=atoms.len() - needle.len() {
        let matches = needle.iter().enumerate().all(|(offset, expected)| {
            matches!(
                atoms.get(index + offset),
                Some(Atom::Char(actual)) if actual.eq_ignore_ascii_case(expected)
            )
        });
        if matches {
            return Some(index);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(value: f64, code: &str) -> String {
        render_number(value, code, false).unwrap()
    }

    #[test]
    fn positive_negative_zero_and_text_sections() {
        assert_eq!(number(12.0, "0.0;[Red](0.0);\"zero\";\"text: \"@"), "12.0");
        assert_eq!(
            number(-12.0, "0.0;[Red](0.0);\"zero\";\"text: \"@"),
            "(12.0)"
        );
        assert_eq!(number(0.0, "0.0;[Red](0.0);\"zero\";\"text: \"@"), "zero");
        assert_eq!(
            render_text("한글", "0;[Red]-0;0;\"문자: \"@").unwrap(),
            "문자: 한글"
        );
    }

    #[test]
    fn conditions_colors_and_unconditional_fallback() {
        let code = "[Blue][>=100]0\" high\";[Red][<0]0\" low\";0\" mid\"";
        assert_eq!(number(150.0, code), "150 high");
        assert_eq!(number(-2.0, code), "-2 low");
        assert_eq!(number(42.0, code), "42 mid");
    }

    #[test]
    fn grouping_optional_digits_percent_and_scaling() {
        assert_eq!(number(1234.5, "#,##0.00"), "1,234.50");
        assert_eq!(number(0.125, "0.0%"), "12.5%");
        assert_eq!(number(12_345_678.0, "0.0,,\"M\""), "12.3M");
        assert_eq!(number(0.0, "#.##"), "");
    }

    #[test]
    fn locale_currency_and_korean_date_literals() {
        assert_eq!(number(1234.0, "[$₩-412]#,##0"), "₩1,234");
        assert_eq!(
            number(45366.0, "[$-412]yyyy\"년\" m\"월\" d\"일\""),
            "2024년 3월 15일"
        );
        assert_eq!(number(45366.0, "[$-412]dddd"), "금요일");
    }

    #[test]
    fn fractions_and_scientific_notation() {
        assert_eq!(number(1.25, "# ?/?"), "1 1/4");
        assert_eq!(number(0.333333333, "??/??"), " 1/ 3");
        assert_eq!(number(1.375, "# ?/8"), "1 3/8");
        assert_eq!(number(1.2, "# ?/10"), "1 2/10");
        assert_eq!(number(12_345.0, "0.00E+00"), "1.23E+04");
        assert_eq!(number(0.0123, "0.0e-0"), "1.2e-2");
    }

    #[test]
    fn date_time_elapsed_ampm_and_fractional_seconds() {
        assert_eq!(
            number(45366.5, "yyyy-mm-dd hh:mm:ss"),
            "2024-03-15 12:00:00"
        );
        assert_eq!(number(45366.5, "m/d/yy h:mm AM/PM"), "3/15/24 12:00 PM");
        assert_eq!(number(1.5, "[h]:mm:ss"), "36:00:00");
        assert_eq!(number(1.0 / 86400.0 * 1.125, "[s].000"), "1.125");
    }

    #[test]
    fn escapes_spacing_fill_and_quoted_semicolons_are_deterministic() {
        assert_eq!(number(42.0, r#"\[0\] "한;글"_)*x"#), "[42] 한;글 ");
        assert_eq!(number(42.0, r#"0\ \%"#), "42 %");
        assert_eq!(number(12.0, r#"000\-00"#), "000-12");
    }

    #[test]
    fn malformed_and_overlong_formats_fail_closed_without_panicking() {
        for malformed in [
            "\"unterminated",
            "[Red",
            "0\\",
            "0_",
            "0*",
            "0;0;0;0;0",
            "[>1][<2]0",
            "[>not-a-number]0",
        ] {
            assert!(
                render_number(1.0, malformed, false).is_none(),
                "{malformed:?}"
            );
        }
        let overlong = "0".repeat(MAX_FORMAT_BYTES + 1);
        assert!(render_number(1.0, &overlong, false).is_none());
    }
}
