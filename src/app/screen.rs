//! What a click can land on, and what a screen may ask the run loop to do.

use crate::columns::ColumnId;
use crate::kube;

/// Something on screen a click can land on.
///
/// The shell keeps a `Vec<(Rect, Target)>` rebuilt every frame and resolves a
/// click to the **last** region containing the point — drawn last is on top,
/// which is what puts a modal over the table under it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    /// A tab, by its index in `config.toml`'s order.
    Tab(usize),
    /// A row of the table, by its index among the rows currently shown.
    Row(usize),
    /// A column header.
    Header(ColumnId),
    SearchField,
    ClearSearch,
    Details,
    /// The text pane under the details.
    TextPane,
    Help,
}

/// What a screen wants the run loop to do next. A screen never talks to the
/// worker or the clipboard itself: it says what it wants and the loop does
/// it, which is what keeps every screen testable without either.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppAction {
    None,
    /// Put this on the clipboard and say `label` in the status bar. The label
    /// never contains what was copied.
    Copy {
        text: String,
        label: String,
    },
    Send(kube::Request),
    Quit,
}

// ponytail: no `Screen` trait. Every tab is the same `ScopeScreen` over a
// different namespace, so `App` indexes a `Vec` of them; a trait object
// would buy nothing.
