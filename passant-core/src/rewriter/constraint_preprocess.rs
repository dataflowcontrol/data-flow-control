//! Expand policy-specific constraint syntax (`UNIQUE`, `NOT UNIQUE`) into SQL aggregates.

/// Rewrite `UNIQUE T.c` / `NOT UNIQUE T.c` into `COUNT(DISTINCT ...)` predicates.
pub fn preprocess_policy_constraint(constraint: &str) -> String {
    let chars: Vec<char> = constraint.chars().collect();
    let mut out = String::with_capacity(constraint.len());
    let mut index = 0usize;
    let mut paren_depth = 0i32;

    while index < chars.len() {
        if let Some(end) = copy_string_literal(&chars, index, &mut out) {
            index = end;
            continue;
        }

        if chars[index] == '(' {
            paren_depth += 1;
            out.push('(');
            index += 1;
            continue;
        }
        if chars[index] == ')' {
            paren_depth = paren_depth.saturating_sub(1);
            out.push(')');
            index += 1;
            continue;
        }

        if paren_depth == 0 {
            if let Some((expanded, next)) = try_expand_not_unique(&chars, index) {
                out.push_str(&expanded);
                index = next;
                continue;
            }
            if let Some((expanded, next)) = try_expand_unique(&chars, index) {
                out.push_str(&expanded);
                index = next;
                continue;
            }
        }

        out.push(chars[index]);
        index += 1;
    }

    out
}

fn try_expand_not_unique(chars: &[char], index: usize) -> Option<(String, usize)> {
    let keyword = "NOT UNIQUE";
    if !keyword_matches(chars, index, keyword) {
        return None;
    }
    if !keyword_has_word_boundary_before(chars, index) {
        return None;
    }
    let after_keyword = index + keyword.len();
    if !chars
        .get(after_keyword)
        .is_none_or(|ch| ch.is_ascii_whitespace())
    {
        return None;
    }
    let (column, next) = parse_qualified_column(chars, after_keyword)?;
    Some((format!("(COUNT(DISTINCT {column}) != 1)"), next))
}

fn try_expand_unique(chars: &[char], index: usize) -> Option<(String, usize)> {
    let keyword = "UNIQUE";
    if !keyword_matches(chars, index, keyword) {
        return None;
    }
    if !keyword_has_word_boundary_before(chars, index) {
        return None;
    }
    let after_keyword = index + keyword.len();
    if !chars
        .get(after_keyword)
        .is_none_or(|ch| ch.is_ascii_whitespace())
    {
        return None;
    }
    let (column, next) = parse_qualified_column(chars, after_keyword)?;
    Some((format!("(COUNT(DISTINCT {column}) = 1)"), next))
}

fn keyword_matches(chars: &[char], index: usize, keyword: &str) -> bool {
    let keyword_chars: Vec<char> = keyword.chars().collect();
    if index + keyword_chars.len() > chars.len() {
        return false;
    }
    chars[index..index + keyword_chars.len()]
        .iter()
        .zip(keyword_chars.iter())
        .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn keyword_has_word_boundary_before(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = chars[index - 1];
    !previous.is_ascii_alphanumeric() && previous != '_'
}

fn parse_qualified_column(chars: &[char], mut index: usize) -> Option<(String, usize)> {
    while index < chars.len() && chars[index].is_ascii_whitespace() {
        index += 1;
    }
    let start = index;
    let mut parts = Vec::new();
    loop {
        let (ident, next) = parse_sql_ident(chars, index)?;
        parts.push(ident);
        index = next;
        if chars.get(index) == Some(&'.') {
            while index < chars.len() && chars[index].is_ascii_whitespace() {
                index += 1;
            }
            if chars.get(index) == Some(&'.') {
                index += 1;
                continue;
            }
        }
        break;
    }
    if parts.len() < 2 || index == start {
        return None;
    }
    Some((parts.join("."), index))
}

fn parse_sql_ident(chars: &[char], mut index: usize) -> Option<(String, usize)> {
    while index < chars.len() && chars[index].is_ascii_whitespace() {
        index += 1;
    }
    if index >= chars.len() {
        return None;
    }
    if chars[index] == '"' {
        let start = index;
        index += 1;
        let mut ident = String::new();
        while index < chars.len() {
            if chars[index] == '"' {
                if chars.get(index + 1) == Some(&'"') {
                    ident.push('"');
                    index += 2;
                    continue;
                }
                index += 1;
                return Some((format!("\"{ident}\""), index));
            }
            ident.push(chars[index]);
            index += 1;
        }
        let _ = start;
        return None;
    }
    let start = index;
    if !is_ident_start(chars[index]) {
        return None;
    }
    index += 1;
    while index < chars.len() && is_ident_continue(chars[index]) {
        index += 1;
    }
    Some((chars[start..index].iter().collect(), index))
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn copy_string_literal(chars: &[char], index: usize, out: &mut String) -> Option<usize> {
    if chars.get(index) != Some(&'\'') {
        return None;
    }
    let mut cursor = index + 1;
    while cursor < chars.len() {
        if chars[cursor] == '\'' {
            if chars.get(cursor + 1) == Some(&'\'') {
                cursor += 2;
                continue;
            }
            cursor += 1;
            out.extend(chars[index..cursor].iter());
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::preprocess_policy_constraint;

    #[test]
    fn expands_unique_and_not_unique() {
        assert_eq!(
            preprocess_policy_constraint("UNIQUE users.email"),
            "(COUNT(DISTINCT users.email) = 1)"
        );
        assert_eq!(
            preprocess_policy_constraint("NOT UNIQUE Receipts.uid"),
            "(COUNT(DISTINCT Receipts.uid) != 1)"
        );
    }

    #[test]
    fn preserves_or_expression_shape() {
        assert_eq!(
            preprocess_policy_constraint(
                "NOT UNIQUE Receipts.uid OR (users.id = 1 AND users.active)"
            ),
            "(COUNT(DISTINCT Receipts.uid) != 1) OR (users.id = 1 AND users.active)"
        );
    }

    #[test]
    fn leaves_plain_sql_unchanged() {
        let sql = "max(foo.id) > 1 AND users.email = 'a@example.com'";
        assert_eq!(preprocess_policy_constraint(sql), sql);
    }
}
