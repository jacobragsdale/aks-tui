//! What every tab reads, and the one place a worker event changes anything:
//! one slot per scope, holding the last read's pods and what the last read
//! said when it failed.

use crate::cache::{CachedScope, Snapshot};
use crate::config::Tab;
use crate::kube::{Event, Pod};
use crate::timestamp::Timestamp;

/// What an event changed, so a screen knows whether to re-filter or leave
/// its cursor alone.
#[derive(Debug, Eq, PartialEq)]
pub enum Applied {
    /// This scope's rows moved.
    Pods(usize),
    /// This scope's read failed: its rows stand and it has a message.
    Failed(usize),
    /// The status bar changed and nothing else.
    Status,
    Nothing,
}

/// One scope's slot.
#[derive(Clone, Debug, Default)]
pub struct ScopeData {
    pub pods: Vec<Pod>,
    /// What the last read said when it failed. The rows are the previous
    /// read's and the pane says so.
    pub error: Option<String>,
    /// When the rows were read, or when the cache they came from was written.
    pub read_at: Option<Timestamp>,
    /// How many reads have answered this run, which tells "nothing has come
    /// back yet" from "the namespace is empty".
    pub reads: usize,
    pub reading: bool,
}

impl ScopeData {
    /// How many pods somebody has to look at: the tab's badge.
    #[must_use]
    pub fn unhealthy(&self) -> usize {
        self.pods.iter().filter(|pod| pod.is_unhealthy()).count()
    }
}

#[derive(Default)]
pub struct Store {
    pub scopes: Vec<ScopeData>,
}

impl Store {
    /// Empty slots, one per tab.
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self {
            scopes: (0..count).map(|_| ScopeData::default()).collect(),
        }
    }

    /// The store as the last run left it: every tab whose scope the cache
    /// holds opens on yesterday's rows, stamped with when they were read.
    #[must_use]
    pub fn from_cache(snapshot: &Snapshot, tabs: &[Tab]) -> Self {
        let mut store = Self::new(tabs.len());
        for (tab, slot) in tabs.iter().zip(&mut store.scopes) {
            if let Some(held) = snapshot.scopes.iter().find(|held| held.scope == tab.scope) {
                slot.pods.clone_from(&held.pods);
                slot.read_at = Some(held.read_at);
            }
        }
        store
    }

    /// What the next save writes: every scope that has been read at all.
    #[must_use]
    pub fn snapshot(&self, tabs: &[Tab]) -> Snapshot {
        Snapshot::new(
            tabs.iter()
                .zip(&self.scopes)
                .filter_map(|(tab, slot)| {
                    Some(CachedScope {
                        scope: tab.scope.clone(),
                        read_at: slot.read_at?,
                        pods: slot.pods.clone(),
                    })
                })
                .collect(),
        )
    }

    #[must_use]
    pub fn scope(&self, index: usize) -> Option<&ScopeData> {
        self.scopes.get(index)
    }

    /// Whether any read is in flight, for the spinner and the poll rate.
    #[must_use]
    pub fn reading(&self) -> bool {
        self.scopes.iter().any(|slot| slot.reading)
    }

    /// Every scope whose last read failed: `qa/dev: message`, for the help.
    #[must_use]
    pub fn problems(&self, tabs: &[Tab]) -> Vec<String> {
        tabs.iter()
            .zip(&self.scopes)
            .filter_map(|(tab, slot)| {
                slot.error
                    .as_ref()
                    .map(|message| format!("{}: {message}", tab.scope.describe()))
            })
            .collect()
    }

    pub fn apply(&mut self, event: Event) -> Applied {
        match event {
            Event::Reading(index) => match self.scopes.get_mut(index) {
                Some(slot) => {
                    slot.reading = true;
                    Applied::Status
                }
                None => Applied::Nothing,
            },
            Event::Pods { scope, pods } => {
                let Some(slot) = self.scopes.get_mut(scope) else {
                    return Applied::Nothing;
                };
                slot.reading = false;
                slot.reads += 1;
                match pods {
                    Ok(pods) => {
                        slot.pods = pods;
                        slot.error = None;
                        slot.read_at = Some(Timestamp::now());
                        Applied::Pods(scope)
                    }
                    Err(message) => {
                        slot.error = Some(message);
                        Applied::Failed(scope)
                    }
                }
            }
            Event::Stopped => Applied::Status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::kube::tests::{crashing, pod};

    fn tabs() -> Vec<Tab> {
        config::parse(config::tests::TWO_CLUSTERS).unwrap().tabs()
    }

    #[test]
    fn a_read_replaces_one_scopes_rows_and_a_failure_keeps_them_with_a_message() {
        let mut store = Store::new(4);
        assert_eq!(store.apply(Event::Reading(0)), Applied::Status);
        assert!(store.reading());
        assert_eq!(
            store.apply(Event::Pods {
                scope: 0,
                pods: Ok(vec![
                    pod("qa", "dev", "a", "Running"),
                    crashing("qa", "dev", "b")
                ]),
            }),
            Applied::Pods(0)
        );
        assert!(!store.reading());
        assert_eq!(store.scopes[0].pods.len(), 2);
        assert_eq!(store.scopes[0].unhealthy(), 1);
        assert_eq!(store.scopes[0].reads, 1);
        assert!(store.scopes[0].read_at.is_some());
        assert!(
            store.scopes[1].pods.is_empty(),
            "the other scopes are untouched"
        );

        let read_at = store.scopes[0].read_at;
        assert_eq!(
            store.apply(Event::Pods {
                scope: 0,
                pods: Err("Unable to connect to the server".into()),
            }),
            Applied::Failed(0)
        );
        assert_eq!(
            store.scopes[0].pods.len(),
            2,
            "yesterday's rows beat no rows"
        );
        assert_eq!(
            store.scopes[0].read_at, read_at,
            "and are not said to be newer"
        );
        assert_eq!(
            store.problems(&tabs()),
            vec!["qa/dev: Unable to connect to the server"]
        );

        store.apply(Event::Pods {
            scope: 0,
            pods: Ok(Vec::new()),
        });
        assert!(
            store.scopes[0].error.is_none(),
            "a read that worked clears it"
        );
        assert!(store.scopes[0].pods.is_empty());
        assert_eq!(
            store.apply(Event::Pods {
                scope: 9,
                pods: Ok(Vec::new())
            }),
            Applied::Nothing,
            "a scope that is not there"
        );
    }

    #[test]
    fn the_cache_round_trips_by_scope_not_by_position() {
        let tabs = tabs();
        let mut store = Store::new(tabs.len());
        store.apply(Event::Pods {
            scope: 3,
            pods: Ok(vec![pod("prod", "prod", "a", "Running")]),
        });
        let snapshot = store.snapshot(&tabs);
        assert_eq!(snapshot.scopes.len(), 1, "only what has been read");

        // The same file, read into a config that lists prod first.
        let reordered = config::parse(
            "[[clusters]]\nname = \"prod\"\nnamespaces = [\"prod\"]\n[[clusters]]\nname = \"qa\"\ncontext = \"aks-qa\"\nnamespaces = [\"dev\"]\n",
        )
        .unwrap()
        .tabs();
        let restored = Store::from_cache(&snapshot, &reordered);
        assert_eq!(
            restored.scopes[0].pods.len(),
            1,
            "prod's rows landed on prod's tab"
        );
        assert!(restored.scopes[0].read_at.is_some());
        assert_eq!(restored.scopes[0].reads, 0, "from the cache is not a read");
        assert!(restored.scopes[1].pods.is_empty());
    }
}
