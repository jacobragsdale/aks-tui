//! The columns a list table shows: which ones, in what order, how wide, and
//! what it drops first when the terminal is too narrow for all of them.
//!
//! Lifted from az-tui: one enum, and one ordered slice of it per kind of
//! list. What a column *starts out* visible as belongs to the table and not
//! to the column, so the slices carry that, and everything else rides on the
//! variant.

use ratatui::layout::{Alignment, Constraint};

/// The two columns the selection marker (`› `) is always given, whether or
/// not the row under the cursor is on screen.
pub const SELECTION_WIDTH: u16 = 2;

/// The scrollbar's own column, at the right edge of every list table. It is
/// reserved whether or not the list overflows, so a table does not shuffle
/// sideways as rows arrive.
pub const SCROLLBAR_WIDTH: u16 = 1;

/// The blank column between two neighbouring cells.
pub const COLUMN_SPACING: u16 = 1;

/// The fewest cells any column is drawn in. Under three there is no room for
/// a value and the sign that it was cut.
pub const MIN_COLUMN_WIDTH: u16 = 3;

/// The fewest cells a flexible column is squeezed to before the table starts
/// dropping optional columns from the right. A pod's name is its deployment's
/// plus two hashes, which is about this much.
pub const MIN_FLEXIBLE_WIDTH: u16 = 24;

/// Every column any list offers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ColumnId {
    // Pods.
    Name,
    Namespace,
    Ready,
    Status,
    Restarts,
    Age,
    Node,
    Ip,
    Owner,
    Image,
}

/// What one column is: what the session file calls it, what its header says,
/// and how much room it wants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnSpec {
    /// The identity. It is what the session file records and what a clicked
    /// header carries back to the screen that drew it, so it is lowercase and
    /// it does not change between releases: renaming one silently drops
    /// somebody's stored width.
    pub key: &'static str,
    /// What the header cell says.
    pub label: &'static str,
    /// What the column opens at. The flexible column has none and takes
    /// whatever is left over instead.
    pub width: u16,
    /// The fewest cells the column is drawn in.
    pub min: u16,
    /// The one column per table that takes the width left over. Its stored
    /// width is ignored.
    pub flexible: bool,
    /// Numbers read better against the right edge of their cell.
    pub align: Alignment,
    /// Whether the column stays whatever happens: the auto-drop never takes a
    /// pinned column away.
    pub pinned: bool,
}

impl ColumnSpec {
    const fn fixed(key: &'static str, label: &'static str, width: u16) -> Self {
        Self {
            key,
            label,
            width,
            min: MIN_COLUMN_WIDTH,
            flexible: false,
            align: Alignment::Left,
            pinned: false,
        }
    }

    /// A count, which is a fixed column read against its right edge.
    const fn count(key: &'static str, label: &'static str, width: u16) -> Self {
        Self {
            align: Alignment::Right,
            ..Self::fixed(key, label, width)
        }
    }

    /// The column a table is really about, which is always the one that takes
    /// the leftover width and always one it will not drop.
    const fn flexible(key: &'static str, label: &'static str, min: u16) -> Self {
        Self {
            width: 0,
            min,
            flexible: true,
            pinned: true,
            ..Self::fixed(key, label, 0)
        }
    }

    /// A fixed column the table keeps however narrow it gets.
    const fn pinned(key: &'static str, label: &'static str, width: u16) -> Self {
        Self {
            pinned: true,
            ..Self::fixed(key, label, width)
        }
    }
}

impl ColumnId {
    /// Every column there is, which is what a key out of the session file is
    /// resolved against.
    pub const ALL: [Self; 10] = [
        Self::Name,
        Self::Namespace,
        Self::Ready,
        Self::Status,
        Self::Restarts,
        Self::Age,
        Self::Node,
        Self::Ip,
        Self::Owner,
        Self::Image,
    ];

    #[must_use]
    pub const fn spec(self) -> ColumnSpec {
        match self {
            Self::Name => ColumnSpec::flexible("name", "Name", MIN_FLEXIBLE_WIDTH),
            Self::Namespace => ColumnSpec::fixed("ns", "Namespace", 14),
            Self::Ready => ColumnSpec::count("ready", "Ready", 5),
            // Wide enough for `CreateContainerConfigError` and its glyph.
            Self::Status => ColumnSpec::pinned("status", "Status", 20),
            Self::Restarts => ColumnSpec::count("restarts", "\u{21bb}", 3),
            Self::Age => ColumnSpec::count("age", "Age", 5),
            Self::Node => ColumnSpec::fixed("node", "Node", 24),
            Self::Ip => ColumnSpec::fixed("ip", "IP", 15),
            Self::Owner => ColumnSpec::fixed("owner", "Owner", 24),
            Self::Image => ColumnSpec::fixed("image", "Image", 32),
        }
    }

    #[must_use]
    pub const fn key(self) -> &'static str {
        self.spec().key
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        self.spec().label
    }

    /// The column that key names. An unknown key comes out of a session file
    /// written by an older build and is dropped.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|column| column.key() == key)
    }
}

/// One column as a table currently has it: its identity, plus the two things
/// a user is allowed to change about it and the session file remembers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnConfig {
    pub id: ColumnId,
    pub visible: bool,
    pub width: u16,
}

impl ColumnConfig {
    #[must_use]
    pub const fn shown(id: ColumnId) -> Self {
        Self {
            id,
            visible: true,
            width: id.spec().width,
        }
    }

    /// A column the table offers but does not open with: room it would rather
    /// spend on the name until somebody asks for it back.
    #[must_use]
    pub const fn hidden(id: ColumnId) -> Self {
        Self {
            visible: false,
            ..Self::shown(id)
        }
    }
}

/// The Pods table, in the order it opens with. The namespace is on the tab,
/// so its column opens hidden and is turned on for a tab over every
/// namespace.
pub const POD_COLUMNS: &[ColumnConfig] = &[
    ColumnConfig::shown(ColumnId::Name),
    ColumnConfig::hidden(ColumnId::Namespace),
    ColumnConfig::shown(ColumnId::Ready),
    ColumnConfig::shown(ColumnId::Status),
    ColumnConfig::shown(ColumnId::Restarts),
    ColumnConfig::shown(ColumnId::Age),
    ColumnConfig::hidden(ColumnId::Owner),
    ColumnConfig::hidden(ColumnId::Node),
    ColumnConfig::hidden(ColumnId::Ip),
    ColumnConfig::hidden(ColumnId::Image),
];

/// One table's columns as they stand: what it opened with, plus whatever the
/// session file has done to them since.
///
// ponytail: nothing edits a layout in v1 — there is no Columns overlay, so
// `visible` and `width` only ever move when the session restores them. An
// overlay would want ticket-tui's `toggle_visible`/`move_column`/`resize`
// back, and they are three matches on `pinned` away.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableLayout {
    pub columns: Vec<ColumnConfig>,
}

impl TableLayout {
    /// A table opened at its defaults.
    #[must_use]
    pub fn new(defaults: &[ColumnConfig]) -> Self {
        Self {
            columns: defaults.to_vec(),
        }
    }

    /// Turns one column on or off, if the table has it.
    pub fn set_visible(&mut self, id: ColumnId, visible: bool) {
        if let Some(column) = self.columns.iter_mut().find(|column| column.id == id) {
            column.visible = visible;
        }
    }

    /// The width the columns and the gaps between them share, inside a pane
    /// `inner_width` wide: what the selection marker and the scrollbar take is
    /// spent before a column sees any of it.
    #[must_use]
    pub const fn available_width(inner_width: u16) -> u16 {
        inner_width
            .saturating_sub(SELECTION_WIDTH)
            .saturating_sub(SCROLLBAR_WIDTH)
    }

    /// The columns this table draws in `available` cells, dropping the
    /// right-most unpinned one for as long as the flexible column would
    /// otherwise fall under its minimum. A pinned column never goes.
    #[must_use]
    pub fn visible_columns(&self, available: u16) -> Vec<ColumnConfig> {
        let mut columns: Vec<_> = self
            .columns
            .iter()
            .copied()
            .filter(|column| column.visible)
            .collect();
        while required_width(&columns) > available {
            let Some(index) = columns.iter().rposition(|column| !column.id.spec().pinned) else {
                break;
            };
            columns.remove(index);
        }
        columns
    }

    #[must_use]
    pub const fn constraint(column: ColumnConfig) -> Constraint {
        if column.id.spec().flexible || column.width == 0 {
            Constraint::Fill(1)
        } else {
            Constraint::Length(column.width)
        }
    }
}

/// What these columns need to draw with the flexible one still readable:
/// every fixed width, the flexible column's own minimum, and a gap between
/// each pair.
fn required_width(columns: &[ColumnConfig]) -> u16 {
    let spacing = COLUMN_SPACING.saturating_mul(
        u16::try_from(columns.len())
            .unwrap_or(u16::MAX)
            .saturating_sub(1),
    );
    columns
        .iter()
        .map(|column| {
            let spec = column.id.spec();
            if spec.flexible {
                spec.min
            } else {
                column.width.max(spec.min)
            }
        })
        .fold(spacing, u16::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the flexible column is left with once the fixed ones and the gaps
    /// between them are paid for.
    fn flexible_width(columns: &[ColumnConfig], available: u16) -> u16 {
        let spacing = COLUMN_SPACING * (columns.len() as u16 - 1);
        let fixed: u16 = columns
            .iter()
            .filter(|column| !column.id.spec().flexible)
            .map(|column| column.width)
            .sum();
        available.saturating_sub(spacing).saturating_sub(fixed)
    }

    fn ids(columns: &[ColumnConfig]) -> Vec<ColumnId> {
        columns.iter().map(|column| column.id).collect()
    }

    #[test]
    fn columns_drop_from_the_right_before_the_name_is_squeezed() {
        let layout = TableLayout::new(POD_COLUMNS);
        for pane in [140_u16, 110, 90, 70, 55] {
            let available = TableLayout::available_width(pane - 2);
            let columns = layout.visible_columns(available);
            let visible = ids(&columns);

            assert_eq!(visible[0], ColumnId::Name, "the pinned columns stay");
            assert!(visible.contains(&ColumnId::Status), "{pane}: {visible:?}");
            assert!(
                flexible_width(&columns, available) >= MIN_FLEXIBLE_WIDTH,
                "{pane} left the name {} wide with {visible:?}",
                flexible_width(&columns, available)
            );
            let mut ordered = visible.clone();
            ordered.sort_by_key(|id| {
                POD_COLUMNS
                    .iter()
                    .position(|column| column.id == *id)
                    .unwrap()
            });
            assert_eq!(ordered, visible, "the columns keep their order");
        }

        assert_eq!(
            ids(&layout.visible_columns(TableLayout::available_width(158))),
            vec![
                ColumnId::Name,
                ColumnId::Ready,
                ColumnId::Status,
                ColumnId::Restarts,
                ColumnId::Age,
            ],
            "a wide enough table keeps every column it opened with"
        );
    }

    #[test]
    fn a_table_too_narrow_for_anything_keeps_the_name_and_the_status() {
        let pods = TableLayout::new(POD_COLUMNS);
        let cramped = TableLayout::available_width(crate::ui::MIN_WIDTH - 2);
        let columns = pods.visible_columns(cramped);
        assert_eq!(ids(&columns), vec![ColumnId::Name, ColumnId::Status]);
        assert!(flexible_width(&columns, cramped) > 0);
        assert_eq!(
            ids(&pods.visible_columns(0)),
            vec![ColumnId::Name, ColumnId::Status],
            "a table with no room at all still says what its rows are"
        );
    }

    #[test]
    fn a_hidden_column_holds_its_place_until_it_is_turned_on() {
        let mut layout = TableLayout::new(POD_COLUMNS);
        assert!(
            !ids(&layout.visible_columns(200)).contains(&ColumnId::Namespace),
            "nobody asked for it"
        );
        layout.set_visible(ColumnId::Namespace, true);
        assert_eq!(
            ids(&layout.visible_columns(200))[..2],
            [ColumnId::Name, ColumnId::Namespace],
            "and it comes back where it always was"
        );
    }

    #[test]
    fn every_key_is_its_own_and_survives_the_round_trip() {
        let mut keys: Vec<&str> = ColumnId::ALL.iter().map(|id| id.key()).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "two columns cannot share a session key");

        for id in ColumnId::ALL {
            assert_eq!(ColumnId::from_key(id.key()), Some(id));
            let spec = id.spec();
            assert!(
                spec.key.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{spec:?} is not a stable lowercase key"
            );
            assert!(
                u16::try_from(spec.label.chars().count()).unwrap() <= spec.width.max(spec.min),
                "{spec:?} cannot show its own header"
            );
        }
        assert_eq!(
            ColumnId::from_key("vault"),
            None,
            "another program's column"
        );
    }

    #[test]
    fn the_flexible_column_fills_and_the_rest_are_what_they_say() {
        let columns = TableLayout::new(POD_COLUMNS).visible_columns(120);
        let constraints: Vec<_> = columns.into_iter().map(TableLayout::constraint).collect();
        assert_eq!(
            constraints[0],
            Constraint::Fill(1),
            "the name takes the rest"
        );
        assert_eq!(constraints[1], Constraint::Length(5));
        assert_eq!(
            TableLayout::available_width(100),
            97,
            "the marker and the scrollbar are spent first"
        );
        assert_eq!(TableLayout::available_width(2), 0, "and cannot overdraw");
    }
}
