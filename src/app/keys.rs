//! Every key, in one table, which is what `?` renders.

/// One row of the help modal, and one case of the dispatch.
pub struct Key {
    /// As the help prints it.
    pub keys: &'static str,
    pub does: &'static str,
}

pub const KEYS: &[Key] = &[
    Key {
        keys: "1-9  [ ]",
        does: "a tab by number; the previous, the next",
    },
    Key {
        keys: "j k  ↑ ↓",
        does: "move the cursor in the focused pane",
    },
    Key {
        keys: "PgUp PgDn",
        does: "a screenful at a time",
    },
    Key {
        keys: "Home End",
        does: "the first row, the last row",
    },
    Key {
        keys: "Tab",
        does: "focus the table or the details pane",
    },
    Key {
        keys: "/",
        does: "search; Esc or Enter keeps the filter, Esc again clears it",
    },
    Key {
        keys: "Ctrl-U",
        does: "clear the search box",
    },
    Key {
        keys: "S",
        does: "sort by the next column; a header click sorts too",
    },
    Key {
        keys: "r",
        does: "read this tab again now",
    },
    Key {
        keys: "?",
        does: "this help",
    },
    Key {
        keys: "q  Ctrl-C",
        does: "quit",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_says_what_it_does_and_the_widest_column_stays_narrow() {
        for key in KEYS {
            assert!(!key.keys.is_empty() && !key.does.is_empty());
            assert!(
                key.keys.chars().count() <= 12,
                "the help's left column is 12 wide: {:?}",
                key.keys
            );
        }
    }
}
