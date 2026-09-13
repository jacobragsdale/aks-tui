//! What every tab reads, and the one place a worker event changes anything:
//! one slot per scope, holding each kind's last read and what the last read
//! said when it failed.

use crate::cache::{CachedScope, Snapshot};
use crate::config::Tab;
use crate::kube::{ConfigMap, Event, K8sEvent, Kind, Pod, SecretMeta};
use crate::timestamp::Timestamp;

/// What an event changed, so a screen knows whether to re-filter or leave
/// its cursor alone.
#[derive(Debug, Eq, PartialEq)]
pub enum Applied {
    /// This scope's rows of this kind moved.
    Rows(usize, Kind),
    /// This scope's read of this kind failed: its rows stand and it has a
    /// message.
    Failed(usize, Kind),
    /// The status bar changed and nothing else.
    Status,
    Nothing,
}

/// One kind's last read for one scope.
#[derive(Clone, Debug)]
pub struct Listing<T> {
    pub rows: Vec<T>,
    /// What the last read said when it failed. The rows are the previous
    /// read's and the pane says so.
    pub error: Option<String>,
    /// When the rows were read, or when the cache they came from was written.
    pub read_at: Option<Timestamp>,
    /// How many reads have answered this run, which tells "nothing has come
    /// back yet" from "the namespace is empty".
    pub reads: usize,
}

impl<T> Default for Listing<T> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            error: None,
            read_at: None,
            reads: 0,
        }
    }
}

impl<T> Listing<T> {
    /// One read landed: its rows replace the last ones, or its message stands
    /// beside them.
    fn apply(&mut self, result: Result<Vec<T>, String>) -> bool {
        self.reads += 1;
        match result {
            Ok(rows) => {
                self.rows = rows;
                self.error = None;
                self.read_at = Some(Timestamp::now());
                true
            }
            Err(message) => {
                self.error = Some(message);
                false
            }
        }
    }
}

/// What one kind's listing says about itself, whatever the kind.
#[derive(Clone, Copy, Debug)]
pub struct ListingState<'a> {
    pub count: usize,
    pub error: Option<&'a String>,
    pub read_at: Option<Timestamp>,
    pub reads: usize,
}

/// One scope's slot: every kind it has been read for.
#[derive(Clone, Debug, Default)]
pub struct ScopeData {
    pub pods: Listing<Pod>,
    pub events: Listing<K8sEvent>,
    pub configmaps: Listing<ConfigMap>,
    pub secrets: Listing<SecretMeta>,
    /// Whether a read of any kind is in flight.
    pub reading: bool,
}

impl ScopeData {
    /// How many pods somebody has to look at: the tab's badge.
    #[must_use]
    pub fn unhealthy(&self) -> usize {
        self.pods
            .rows
            .iter()
            .filter(|pod| pod.is_unhealthy())
            .count()
    }

    /// One kind's listing, whatever the kind.
    #[must_use]
    pub fn listing(&self, kind: Kind) -> ListingState<'_> {
        fn state<T>(listing: &Listing<T>) -> ListingState<'_> {
            ListingState {
                count: listing.rows.len(),
                error: listing.error.as_ref(),
                read_at: listing.read_at,
                reads: listing.reads,
            }
        }
        match kind {
            Kind::Pods => state(&self.pods),
            Kind::Events => state(&self.events),
            Kind::ConfigMaps => state(&self.configmaps),
            Kind::Secrets => state(&self.secrets),
        }
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
    /// holds opens on yesterday's pods, stamped with when they were read.
    #[must_use]
    pub fn from_cache(snapshot: &Snapshot, tabs: &[Tab]) -> Self {
        let mut store = Self::new(tabs.len());
        for (tab, slot) in tabs.iter().zip(&mut store.scopes) {
            if let Some(held) = snapshot.scopes.iter().find(|held| held.scope == tab.scope) {
                slot.pods.rows.clone_from(&held.pods);
                slot.pods.read_at = Some(held.read_at);
            }
        }
        store
    }

    /// What the next save writes: every scope's pods, when it has any read.
    /// Pods only: events are gone in an hour, configmaps can be large, and a
    /// secret's shape is nobody's business on disk.
    #[must_use]
    pub fn snapshot(&self, tabs: &[Tab]) -> Snapshot {
        Snapshot::new(
            tabs.iter()
                .zip(&self.scopes)
                .filter_map(|(tab, slot)| {
                    Some(CachedScope {
                        scope: tab.scope.clone(),
                        read_at: slot.pods.read_at?,
                        pods: slot.pods.rows.clone(),
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

    /// Every scope whose last read of some kind failed: `qa/dev events:
    /// message`, for the help.
    #[must_use]
    pub fn problems(&self, tabs: &[Tab]) -> Vec<String> {
        let mut problems = Vec::new();
        for (tab, slot) in tabs.iter().zip(&self.scopes) {
            for kind in Kind::ALL {
                if let Some(message) = slot.listing(kind).error {
                    problems.push(format!(
                        "{} {}: {message}",
                        tab.scope.describe(),
                        kind.noun()
                    ));
                }
            }
        }
        problems
    }

    pub fn apply(&mut self, event: Event) -> Applied {
        let landed = |scope: usize, kind: Kind, ok: bool| {
            if ok {
                Applied::Rows(scope, kind)
            } else {
                Applied::Failed(scope, kind)
            }
        };
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
                landed(scope, Kind::Pods, slot.pods.apply(pods))
            }
            Event::Events { scope, events } => {
                let Some(slot) = self.scopes.get_mut(scope) else {
                    return Applied::Nothing;
                };
                slot.reading = false;
                landed(scope, Kind::Events, slot.events.apply(events))
            }
            Event::ConfigMaps { scope, configmaps } => {
                let Some(slot) = self.scopes.get_mut(scope) else {
                    return Applied::Nothing;
                };
                slot.reading = false;
                landed(scope, Kind::ConfigMaps, slot.configmaps.apply(configmaps))
            }
            Event::Secrets { scope, secrets } => {
                let Some(slot) = self.scopes.get_mut(scope) else {
                    return Applied::Nothing;
                };
                slot.reading = false;
                landed(scope, Kind::Secrets, slot.secrets.apply(secrets))
            }
            // Straight to the screen that asked; nothing here keeps them.
            Event::LogLines { .. }
            | Event::Text { .. }
            | Event::Deleted { .. }
            | Event::Acted { .. }
            | Event::Owner { .. }
            | Event::SecretValue { .. } => Applied::Nothing,
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
            Applied::Rows(0, Kind::Pods)
        );
        assert!(!store.reading());
        assert_eq!(store.scopes[0].pods.rows.len(), 2);
        assert_eq!(store.scopes[0].unhealthy(), 1);
        assert_eq!(store.scopes[0].pods.reads, 1);
        assert!(store.scopes[0].pods.read_at.is_some());
        assert!(
            store.scopes[1].pods.rows.is_empty(),
            "the other scopes are untouched"
        );

        let read_at = store.scopes[0].pods.read_at;
        assert_eq!(
            store.apply(Event::Pods {
                scope: 0,
                pods: Err("Unable to connect to the server".into()),
            }),
            Applied::Failed(0, Kind::Pods)
        );
        assert_eq!(
            store.scopes[0].pods.rows.len(),
            2,
            "yesterday's rows beat no rows"
        );
        assert_eq!(
            store.scopes[0].pods.read_at, read_at,
            "and are not said to be newer"
        );
        assert_eq!(
            store.apply(Event::Secrets {
                scope: 3,
                secrets: Err("Error from server (Forbidden): secrets is forbidden".into()),
            }),
            Applied::Failed(3, Kind::Secrets)
        );
        assert_eq!(
            store.problems(&tabs()),
            vec![
                "qa/dev pods: Unable to connect to the server",
                "prod/prod secrets: Error from server (Forbidden): secrets is forbidden"
            ]
        );

        store.apply(Event::Pods {
            scope: 0,
            pods: Ok(Vec::new()),
        });
        assert!(
            store.scopes[0].pods.error.is_none(),
            "a read that worked clears it"
        );
        assert!(store.scopes[0].pods.rows.is_empty());
        assert_eq!(
            store.apply(Event::Pods {
                scope: 9,
                pods: Ok(Vec::new())
            }),
            Applied::Nothing,
            "a scope that is not there"
        );
        let events = store.scopes[0].listing(Kind::Events);
        assert_eq!((events.count, events.reads), (0, 0));
    }

    #[test]
    fn the_cache_round_trips_by_scope_not_by_position_and_holds_pods_only() {
        let tabs = tabs();
        let mut store = Store::new(tabs.len());
        store.apply(Event::Pods {
            scope: 3,
            pods: Ok(vec![pod("prod", "prod", "a", "Running")]),
        });
        store.apply(Event::Events {
            scope: 3,
            events: Ok(Vec::new()),
        });
        let snapshot = store.snapshot(&tabs);
        assert_eq!(snapshot.scopes.len(), 1, "only what has been read");
        let written = serde_json::to_string(&snapshot).unwrap();
        assert!(!written.contains("events"), "{written}");
        assert!(!written.contains("secret"), "{written}");

        // The same file, read into a config that lists prod first.
        let reordered = config::parse(
            "[[clusters]]\nname = \"prod\"\nnamespaces = [\"prod\"]\n[[clusters]]\nname = \"qa\"\ncontext = \"aks-qa\"\nnamespaces = [\"dev\"]\n",
        )
        .unwrap()
        .tabs();
        let restored = Store::from_cache(&snapshot, &reordered);
        assert_eq!(
            restored.scopes[0].pods.rows.len(),
            1,
            "prod's rows landed on prod's tab"
        );
        assert!(restored.scopes[0].pods.read_at.is_some());
        assert_eq!(
            restored.scopes[0].pods.reads, 0,
            "from the cache is not a read"
        );
        assert!(restored.scopes[1].pods.rows.is_empty());
    }
}
