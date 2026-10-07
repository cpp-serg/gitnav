//! Matching a query against a row's text, for search and filter.

/// How a query matches: fuzzy (its characters in order, gaps allowed) or as a substring, and
/// whether case matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub fuzzy: bool,
    pub ignore_case: bool,
}

impl Default for Mode {
    fn default() -> Self {
        Mode { fuzzy: true, ignore_case: true }
    }
}

impl Mode {
    pub fn describe(self) -> String {
        format!(
            "{}, {}",
            if self.fuzzy { "fuzzy" } else { "exact" },
            if self.ignore_case { "ignore case" } else { "match case" }
        )
    }
}

/// The positions (char indices into `text`) of the characters that match `query`, or `None`.
/// An empty query matches nothing. A fuzzy query that also occurs as a substring is shown at
/// that substring, which reads better than letters picked here and there.
pub fn find(query: &str, text: &str, mode: Mode) -> Option<Vec<usize>> {
    let fold = |c: char| if mode.ignore_case { c.to_lowercase().next().unwrap_or(c) } else { c };
    let q: Vec<char> = query.chars().map(fold).collect();
    if q.is_empty() {
        return None;
    }
    let t: Vec<char> = text.chars().map(fold).collect();

    if let Some(start) = t.windows(q.len()).position(|w| w == q.as_slice()) {
        return Some((start..start + q.len()).collect());
    }
    if !mode.fuzzy {
        return None;
    }
    let mut positions = Vec::with_capacity(q.len());
    let mut next = q.iter().peekable();
    for (i, c) in t.iter().enumerate() {
        if next.peek() == Some(&c) {
            positions.push(i);
            next.next();
        }
    }
    (next.peek().is_none()).then_some(positions)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FUZZY: Mode = Mode { fuzzy: true, ignore_case: true };
    const EXACT: Mode = Mode { fuzzy: false, ignore_case: true };

    #[test]
    fn substring_first() {
        assert_eq!(find("lab", "Update home-lab.md", FUZZY), Some(vec![12, 13, 14]));
        assert_eq!(find("LAB", "Update home-lab.md", EXACT), Some(vec![12, 13, 14]));
    }

    #[test]
    fn fuzzy_in_order() {
        assert_eq!(find("uhl", "Update home-lab", FUZZY), Some(vec![0, 7, 12]));
        assert_eq!(find("uhl", "Update home-lab", EXACT), None);
        assert_eq!(find("lhu", "Update home-lab", FUZZY), None);
    }

    #[test]
    fn case() {
        let strict = Mode { fuzzy: true, ignore_case: false };
        assert!(find("update", "Update", FUZZY).is_some());
        assert!(find("update", "Update", strict).is_none());
        assert!(find("Update", "Update", strict).is_some());
    }

    #[test]
    fn edges() {
        assert_eq!(find("", "anything", FUZZY), None);
        assert_eq!(find("longer", "long", FUZZY), None);
        assert_eq!(find("é", "café", FUZZY), Some(vec![3]));
    }
}
