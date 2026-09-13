//! `key:value` in the search box.
//!
//! A token is a field filter when its key is one the list knows; anything
//! else is a word and goes to [`crate::search`] to be matched literally.
//! That is deliberate: `orders-api:1.2` should search for itself, not fail
//! because `orders-api` is not a field.
//!
//! Every filter and every word is ANDed. There is no `or`, no negation and
//! no grouping.
//!
// ponytail: values do not take quotes, so `reason:Back-off restarting` is
// two tokens. A quoted value wants one pass of a small tokeniser here;
// nobody has needed one in a search box whose whole job is finding a name.

/// A parsed query: the words to match literally, and the fields to test.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Query {
    pub words: Vec<String>,
    pub fields: Vec<(String, String)>,
}

impl Query {
    /// Splits a raw query into words and `key:value` pairs. `known` says
    /// which keys this list understands; a token with any other key is a
    /// word.
    #[must_use]
    pub fn parse(raw: &str, known: &[&str]) -> Self {
        let mut query = Self::default();
        for token in raw.split_whitespace() {
            match token.split_once(':') {
                Some((key, value)) if known.iter().any(|held| held.eq_ignore_ascii_case(key)) => {
                    // A key the list knows with nothing after the colon is
                    // nothing yet: half-typing a filter must not empty the
                    // table on the way to typing it.
                    if !value.is_empty() {
                        query
                            .fields
                            .push((key.to_ascii_lowercase(), value.to_owned()));
                    }
                }
                _ => query.words.push(token.to_owned()),
            }
        }
        query
    }
}

/// Whether a cell contains what was asked for, ignoring case. The shape
/// every plain `key:` filter takes.
#[must_use]
pub fn contains(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PODS: &[&str] = &["status", "owner", "app", "node"];

    #[test]
    fn a_token_is_a_field_only_when_the_list_knows_the_key() {
        let query = Query::parse("orders status:crash owner:orders-api", PODS);
        assert_eq!(query.words, ["orders"]);
        assert_eq!(
            query.fields,
            [
                ("status".to_owned(), "crash".to_owned()),
                ("owner".to_owned(), "orders-api".to_owned())
            ]
        );
    }

    #[test]
    fn a_colon_in_something_that_is_not_a_filter_stays_a_word() {
        let query = Query::parse("orders-api:1.2.3 foo:bar", PODS);
        assert_eq!(
            query.words,
            ["orders-api:1.2.3", "foo:bar"],
            "an unknown key is not a mistake, it is what was typed"
        );
        assert!(query.fields.is_empty());

        let query = Query::parse("status:", PODS);
        assert!(
            query.words.is_empty() && query.fields.is_empty(),
            "a key with nothing after it is nothing yet, not a word that empties the table"
        );
    }

    #[test]
    fn a_plain_field_is_a_case_insensitive_substring() {
        assert!(contains("CrashLoopBackOff", "crash"));
        assert!(!contains("Running", "crash"));
    }
}
