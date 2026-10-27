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

/// Compares two deb, rpm or pacman versions, `[epoch:]upstream[-revision]`.
/// The epoch compares numerically (missing is 0), then the upstream part and
/// the revision (missing is empty) compare segment by segment: `~` sorts
/// before everything, even the end of the string, digit runs compare
/// numerically, a digit run is newer than a letter run, and other characters
/// only separate segments.
pub fn compare_package(a: &str, b: &str) -> Ordering {
    let (epoch_a, rest_a) = split_epoch(a);
    let (epoch_b, rest_b) = split_epoch(b);
    let (upstream_a, revision_a) = rest_a.rsplit_once('-').unwrap_or((rest_a, ""));
    let (upstream_b, revision_b) = rest_b.rsplit_once('-').unwrap_or((rest_b, ""));
    compare_numeric(epoch_a, epoch_b)
        .then_with(|| compare_segments(upstream_a, upstream_b))
        .then_with(|| compare_segments(revision_a, revision_b))
}

/// The digits before the first `:` and the rest; no epoch is `"0"`.
fn split_epoch(v: &str) -> (&str, &str) {
    match v.split_once(':') {
        Some((epoch, rest)) if is_digits(epoch) => (epoch, rest),
        _ => ("0", v),
    }
}

fn compare_segments(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        a = skip_separators(a);
        b = skip_separators(b);
        match (a.first() == Some(&b'~'), b.first() == Some(&b'~')) {
            (true, true) => {
                a = &a[1..];
                b = &b[1..];
                continue;
            }
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        let (Some(&x), Some(&y)) = (a.first(), b.first()) else {
            return a.len().cmp(&b.len());
        };
        let numeric = x.is_ascii_digit();
        if numeric != y.is_ascii_digit() {
            return if numeric {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let (run_a, tail_a) = take_run(a, numeric);
        let (run_b, tail_b) = take_run(b, numeric);
        let ord = if numeric {
            compare_numeric(run_a, run_b)
        } else {
            run_a.cmp(run_b)
        };
        if ord != Ordering::Equal {
            return ord;
        }
        a = tail_a;
        b = tail_b;
    }
}

fn skip_separators(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|b| b.is_ascii_alphanumeric() || *b == b'~')
        .unwrap_or(s.len());
    &s[start..]
}

/// The leading run of digits (or of letters) and what follows it.
fn take_run(s: &[u8], numeric: bool) -> (&str, &[u8]) {
    let end = s
        .iter()
        .position(|b| {
            if numeric {
                !b.is_ascii_digit()
            } else {
                !b.is_ascii_alphabetic()
            }
        })
        .unwrap_or(s.len());
    // Runs are ASCII, so the slice is valid UTF-8.
    (std::str::from_utf8(&s[..end]).unwrap_or(""), &s[end..])
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
    fn package_versions_compare() {
        use std::cmp::Ordering::{Greater, Less};
        assert_eq!(compare_package("1.2.10-1", "1.2.9-1"), Greater);
        assert_eq!(compare_package("1.2.3-1", "1.2.3-1"), Equal);
        assert_eq!(compare_package("1.2.3-2", "1.2.3-1"), Greater);
        assert_eq!(compare_package("1:1.0-1", "2.0-1"), Greater);
        assert_eq!(compare_package("0:1.0-1", "1.0-1"), Equal);
        assert_eq!(compare_package("1.0~rc1-1", "1.0-1"), Less);
        assert_eq!(compare_package("1.0-1.fc40", "1.0-1.fc40"), Equal);
        assert_eq!(compare_package("1.0a", "1.0"), Greater);
        assert_eq!(compare_package("1.0.1", "1.0a"), Greater);
    }

    #[test]
    fn package_compare_is_antisymmetric_and_never_panics() {
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
            "1:",
            ":1",
            "1.0~",
            "~",
            "2:1.0-3",
            "é-ü",
        ];
        for a in inputs {
            for b in inputs {
                assert_eq!(
                    compare_package(a, b),
                    compare_package(b, a).reverse(),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn build_metadata_does_not_order_versions() {
        assert_eq!(compare("1.0.0+build", "1.0.0"), Equal);
    }
}
