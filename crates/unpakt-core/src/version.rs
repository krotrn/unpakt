use std::cmp::Ordering;

/// Compares two release tags leniently. Tags that parse as semver (after
/// padding to three numeric parts) compare as semver; anything else is split
/// into digit and non-digit runs, compared numerically and lexically.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (a, b) = (strip_tag(a), strip_tag(b));
    match (parse_semver(a), parse_semver(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => compare_chunks(a, b),
    }
}

/// True when `candidate` sorts strictly after `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare(candidate, current) == Ordering::Greater
}

fn strip_tag(tag: &str) -> &str {
    let tag = tag.strip_prefix("release-").unwrap_or(tag);
    tag.strip_prefix(['v', 'V']).unwrap_or(tag)
}

fn parse_semver(tag: &str) -> Option<semver::Version> {
    // Build metadata is ignored when ordering, as in semver's own precedence rules.
    let tag = tag.split_once('+').map_or(tag, |(head, _)| head);
    let core_end = tag.find(['-', '+']).unwrap_or(tag.len());
    let (core, suffix) = tag.split_at(core_end);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() > 3 || parts.iter().any(|p| !is_digits(p)) {
        return None;
    }
    let mut padded = core.to_string();
    for _ in parts.len()..3 {
        padded.push_str(".0");
    }
    padded.push_str(suffix);
    semver::Version::parse(&padded).ok()
}

fn compare_chunks(a: &str, b: &str) -> Ordering {
    let (mut a_chunks, mut b_chunks) = (chunks(a), chunks(b));
    loop {
        match (a_chunks.next(), b_chunks.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = if is_digits(x) && is_digits(y) {
                    compare_numeric(x, y)
                } else {
                    x.cmp(y)
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Splits into maximal runs of ASCII digits and non-digits.
fn chunks(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let digit = rest.chars().next()?.is_ascii_digit();
        let end = rest
            .find(|c: char| c.is_ascii_digit() != digit)
            .unwrap_or(rest.len());
        let (chunk, tail) = rest.split_at(end);
        rest = tail;
        Some(chunk)
    })
}

/// Non-empty and all ASCII digits.
fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn compare_numeric(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
    (a.len(), a).cmp(&(b.len(), b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::Equal;

    #[test]
    fn versions() {
        assert!(is_newer("v1.10.0", "v1.9.9"));
        assert_eq!(compare("1.6", "1.6.0"), Equal);
        assert!(is_newer("1.0.0", "1.0.0-rc1"));
        assert!(is_newer("2024.10.01", "2024.9.30"));
        assert!(!is_newer("nightly", "nightly"));
    }

    #[test]
    fn compare_is_antisymmetric_and_never_panics() {
        let inputs = [
            "",
            "v",
            "release-",
            "1..2",
            "99999999999999999999.0.0",
            "1.0.0",
            "1.0.0-rc1",
            "1.0.0+build",
            "nightly",
            "2024.10.01",
            "v1.10.0",
        ];
        for a in inputs {
            for b in inputs {
                assert_eq!(compare(a, b), compare(b, a).reverse(), "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn build_metadata_does_not_order_versions() {
        assert_eq!(compare("1.0.0+build", "1.0.0"), Equal);
    }
}
