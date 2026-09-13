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
    /// One of the toolbar buttons in the details pane.
    Button(Button),
    /// A modal's yes.
    Confirm,
    /// A modal's body: a click there does nothing.
    Modal,
    /// Anywhere that closes a modal.
    Dismiss,
    Help,
}

/// The buttons in the details pane's toolbar. Each stands for the key it
/// names, so clicking one is pressing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Button {
    Logs,
    Bash,
    Restart,
    Scale,
    Describe,
    Yaml,
}

impl Button {
    pub const ALL: [Self; 6] = [
        Self::Logs,
        Self::Bash,
        Self::Restart,
        Self::Scale,
        Self::Describe,
        Self::Yaml,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Logs => "Logs",
            Self::Bash => "Bash",
            Self::Restart => "Restart",
            Self::Scale => "Scale",
            Self::Describe => "Describe",
            Self::Yaml => "YAML",
        }
    }
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
    /// Hand the terminal to `kubectl exec -it` and take it back after.
    Exec {
        context: String,
        namespace: String,
        pod: String,
        container: Option<String>,
    },
    Quit,
}

// ponytail: no `Screen` trait. Every tab is the same `ScopeScreen` over a
// different namespace, so `App` indexes a `Vec` of them; a trait object
// would buy nothing.
