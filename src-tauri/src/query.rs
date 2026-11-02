/// Turn a human query into an FTS5 MATCH string.
///
/// Tokens keep decimal points, so `42.99` stays one word. Each token is quoted
/// so the search cannot be read as an FTS operator.
pub fn to_fts_query(raw: &str) -> Option<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in raw.chars() {
        if ch.is_alphanumeric() || ch == '.' {
            current.push(ch);
        } else if !current.is_empty() {
            push_token(&mut tokens, &current);
            current.clear();
        }
    }
    if !current.is_empty() {
        push_token(&mut tokens, &current);
    }

    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" AND "))
    }
}

fn push_token(tokens: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim_matches('.');
    if trimmed.is_empty() || !trimmed.chars().any(|ch| ch.is_alphanumeric()) {
        return;
    }
    let escaped = trimmed.replace('"', "");
    tokens.push(format!("\"{escaped}\""));
}

#[cfg(test)]
mod tests {
    use super::to_fts_query;

    #[test]
    fn joins_words_with_and() {
        assert_eq!(
            to_fts_query("dishwasher warranty"),
            Some("\"dishwasher\" AND \"warranty\"".to_string())
        );
    }

    #[test]
    fn keeps_decimal_amounts() {
        assert_eq!(
            to_fts_query("total 42.99"),
            Some("\"total\" AND \"42.99\"".to_string())
        );
    }

    #[test]
    fn drops_punctuation_only_queries() {
        assert_eq!(to_fts_query("... ---"), None);
        assert_eq!(to_fts_query("   "), None);
    }

    #[test]
    fn quotes_do_not_become_operators() {
        assert_eq!(
            to_fts_query("\"Mara Ellison\""),
            Some("\"Mara\" AND \"Ellison\"".to_string())
        );
    }
}
