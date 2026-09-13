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
        does: "a screenful at a time; Home and End the ends",
    },
    Key {
        keys: "Tab",
        does: "focus the table or the pane under the details",
    },
    Key {
        keys: "/",
        does: "search; in the text pane, filter its lines; Esc keeps it, Esc again clears it",
    },
    Key {
        keys: "p e m s",
        does: "Pods, Events, ConfigMaps, Secrets; e on a pod is that pod's events",
    },
    Key {
        keys: "Enter  l",
        does: "pods: the log, following, in the text pane; again closes it. Events: the pod. ConfigMaps and Secrets: the key's value",
    },
    Key {
        keys: "d  v",
        does: "describe / YAML of what is under the cursor; v on a configmap or secret is the key's value",
    },
    Key {
        keys: "P  C",
        does: "the log before the last restart; the pod's next container",
    },
    Key {
        keys: "End  z",
        does: "follow the log again; the text pane alone, and back",
    },
    Key {
        keys: "b",
        does: "a shell in the pod: bash, or sh when there is none",
    },
    Key {
        keys: "x  X",
        does: "restart the pod (delete it; its owner replaces it) / rollout-restart its owner",
    },
    Key {
        keys: "=",
        does: "scale the pod's deployment or statefulset",
    },
    Key {
        keys: "y  Y",
        does: "copy the name — on a configmap or secret, the key's value, unseen; copy the kubectl line for what the pane shows",
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
