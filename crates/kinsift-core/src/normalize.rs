//! Text normalization used for grouping.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Normalize a title for grouping: case-folded, accents stripped,
/// apostrophes and dots removed, `&` read as "and", other punctuation
/// treated as whitespace.
///
/// "Ferris's Agents of P.R.I.S.M." and "Ferriss Agents of PRISM" map to
/// the same key, as do "Kumo ½" and "Kumo 1 2".
pub fn title_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfkd() {
        if is_combining_mark(c) {
            continue;
        }
        match c {
            '\'' | '’' | '‘' | '`' | '.' => {}
            '&' => out.push_str(" and "),
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::title_key;

    #[test]
    fn folds_variants_together() {
        assert_eq!(title_key("Ferris's Agents of P.R.I.S.M."), title_key("Ferriss Agents of PRISM"));
        assert_eq!(title_key("Moths & Kettles"), title_key("Moths and Kettles"));
        assert_eq!(title_key("Kumo ½"), title_key("Kumo 1 2"));
        assert_eq!(title_key("Café Noël"), "cafe noel");
        assert_eq!(title_key("  Foo -  Bar "), "foo bar");
    }
}
