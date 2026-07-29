//! Turning arbitrary user text into a valid FTS5 `MATCH` expression.

/// Convert a raw query into a safe FTS5 `MATCH` expression.
///
/// FTS5 treats a lot of ordinary punctuation as operator syntax — `?`, `,`,
/// `'`, `$`, `.`, `-` and more — so a natural-language question passed through
/// unchanged is frequently not a valid expression at all, and SQLite answers
/// with a syntax error rather than no results. Bound parameters do not help:
/// the value still has to parse as a MATCH expression once bound.
///
/// Each word token is extracted and wrapped in double quotes, which also stops
/// a token like `or`, `and` or `near` being read as an operator. Tokens are
/// joined with `OR` so any of them can match; BM25 still ranks by term
/// importance, so common words do not dominate the results.
///
/// Returns an empty string when there is nothing searchable. Callers must treat
/// that as "no results" rather than passing it to `MATCH`, which would itself
/// be a syntax error.
pub fn sanitize_fts_query(query: &str) -> String {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        // FTS5 escapes a literal double quote by doubling it.
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();

    tokens.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_become_quoted_alternatives() {
        assert_eq!(sanitize_fts_query("hello world"), "\"hello\" OR \"world\"");
    }

    #[test]
    fn punctuation_that_fts5_would_choke_on_is_stripped() {
        // A natural-language question: every one of ? ' , would otherwise be
        // read as operator syntax.
        assert_eq!(
            sanitize_fts_query("what's the plan, exactly?"),
            "\"what\" OR \"s\" OR \"the\" OR \"plan\" OR \"exactly\""
        );
    }

    #[test]
    fn operator_words_are_quoted_rather_than_parsed() {
        assert_eq!(
            sanitize_fts_query("this AND that NEAR other"),
            "\"this\" OR \"AND\" OR \"that\" OR \"NEAR\" OR \"other\""
        );
    }

    #[test]
    fn underscores_survive_as_part_of_a_token() {
        // Identifiers like `memory_tags` should stay whole.
        assert_eq!(sanitize_fts_query("memory_tags"), "\"memory_tags\"");
    }

    #[test]
    fn a_query_with_no_word_characters_yields_nothing() {
        assert_eq!(sanitize_fts_query("?!  ..."), "");
        assert_eq!(sanitize_fts_query(""), "");
    }

    #[test]
    fn embedded_quotes_are_doubled() {
        assert_eq!(sanitize_fts_query("say \"hi\""), "\"say\" OR \"hi\"");
    }
}
