//! The download list's columns: order, widths and which are shown.
//!
//! Widths are stored in pixels. With "fit to window" on, they are scaled so
//! the visible columns fill the list exactly; resizing then trades width
//! between a column and its right-hand neighbour. With it off, each column
//! keeps its own width. Plain Rust, so it is unit-tested without a window.

use serde::{Deserialize, Serialize};

/// A column of the download list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Name,
    Size,
    Progress,
    Speed,
    TimeLeft,
    Status,
}

impl Column {
    pub const ALL: [Column; 6] = [
        Column::Name,
        Column::Size,
        Column::Progress,
        Column::Speed,
        Column::TimeLeft,
        Column::Status,
    ];

    /// Narrowest a column may get, in pixels.
    pub fn min_width(self) -> f32 {
        match self {
            Column::Name => 160.0,
            Column::Progress => 110.0,
            _ => 64.0,
        }
    }

    fn default_width(self) -> f32 {
        match self {
            Column::Name => 320.0,
            Column::Size => 120.0,
            Column::Progress => 190.0,
            Column::Speed => 100.0,
            Column::TimeLeft => 100.0,
            Column::Status => 130.0,
        }
    }

    /// The name column is always shown.
    pub fn can_hide(self) -> bool {
        self != Column::Name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ColumnState {
    pub column: Column,
    pub width: f32,
    pub visible: bool,
}

/// All columns in display order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Columns {
    pub items: Vec<ColumnState>,
    /// Scale the columns to fill the list's width.
    pub fit: bool,
}

impl Default for Columns {
    fn default() -> Self {
        Self {
            items: Column::ALL
                .iter()
                .map(|&column| ColumnState { column, width: column.default_width(), visible: true })
                .collect(),
            fit: true,
        }
    }
}

impl Columns {
    /// Repair a loaded value: every column exactly once, sane widths, Name shown.
    pub fn validated(mut self) -> Self {
        let mut seen = Vec::new();
        self.items.retain(|s| {
            let fresh = !seen.contains(&s.column);
            seen.push(s.column);
            fresh
        });
        for column in Column::ALL {
            if !self.items.iter().any(|s| s.column == column) {
                self.items.push(ColumnState {
                    column,
                    width: column.default_width(),
                    visible: true,
                });
            }
        }
        for s in &mut self.items {
            if !s.width.is_finite() || s.width < s.column.min_width() {
                s.width = s.column.default_width().max(s.column.min_width());
            }
            if !s.column.can_hide() {
                s.visible = true;
            }
        }
        self
    }

    pub fn visible(&self) -> impl Iterator<Item = &ColumnState> {
        self.items.iter().filter(|s| s.visible)
    }

    /// Visible columns with the widths to draw them at, given the space the
    /// columns share (`available`, already without gaps and padding).
    pub fn layout(&self, available: f32) -> Vec<(Column, f32)> {
        let shown: Vec<_> = self.visible().collect();
        if !self.fit {
            return shown.iter().map(|s| (s.column, s.width)).collect();
        }
        let total: f32 = shown.iter().map(|s| s.width).sum();
        let scale = if total > 0.0 { available.max(0.0) / total } else { 1.0 };
        shown.iter().map(|s| (s.column, (s.width * scale).max(s.column.min_width()))).collect()
    }

    /// Drag the right edge of `column` by `dx` pixels. With fit on, the next
    /// visible column gives or takes the same amount, so the total stays put.
    pub fn resize(&mut self, column: Column, dx: f32, available: f32) {
        let shown: Vec<usize> = (0..self.items.len()).filter(|&i| self.items[i].visible).collect();
        let Some(pos) = shown.iter().position(|&i| self.items[i].column == column) else {
            return;
        };
        let i = shown[pos];
        if !self.fit {
            let s = &mut self.items[i];
            s.width = (s.width + dx).max(s.column.min_width());
            return;
        }
        let Some(&j) = shown.get(pos + 1) else { return };
        let laid = self.layout(available);
        let (wi, wj) = (laid[pos].1, laid[pos + 1].1);
        let (min_i, min_j) = (self.items[i].column.min_width(), self.items[j].column.min_width());
        let dx = dx.clamp(min_i - wi, wj - min_j);
        let total: f32 = shown.iter().map(|&k| self.items[k].width).sum();
        let scale = if available > 0.0 { total / available } else { 1.0 };
        self.items[i].width = (wi + dx) * scale;
        self.items[j].width = (wj - dx) * scale;
    }

    /// Move `column` to position `to` among the visible columns.
    pub fn move_to(&mut self, column: Column, to: usize) {
        let Some(from) = self.items.iter().position(|s| s.column == column) else { return };
        let item = self.items.remove(from);
        // Translate the visible position into an index of `items`.
        let mut seen = 0;
        let mut at = self.items.len();
        for (k, s) in self.items.iter().enumerate() {
            if !s.visible {
                continue;
            }
            if seen == to {
                at = k;
                break;
            }
            seen += 1;
        }
        self.items.insert(at, item);
    }

    /// One step left (`-1`) or right (`1`) among the visible columns.
    pub fn shift(&mut self, column: Column, step: i32) {
        let order: Vec<Column> = self.visible().map(|s| s.column).collect();
        let Some(pos) = order.iter().position(|c| *c == column) else { return };
        let to = pos as i32 + step;
        if to < 0 || to as usize >= order.len() {
            return;
        }
        self.move_to(column, to as usize);
    }

    pub fn toggle(&mut self, column: Column) {
        if let Some(s) = self.items.iter_mut().find(|s| s.column == column)
            && column.can_hide()
        {
            s.visible = !s.visible;
        }
    }

    /// Set a column's width to fit its content (`content` pixels).
    pub fn size_to(&mut self, column: Column, content: f32) {
        if let Some(s) = self.items.iter_mut().find(|s| s.column == column) {
            s.width = content.max(column.min_width());
        }
    }

    /// The visible position whose middle is nearest to `x`, for dropping a
    /// dragged column.
    pub fn drop_index(&self, x: f32, available: f32, gap: f32) -> usize {
        let mut left = 0.0;
        let mut index = 0;
        for (column_index, (_, width)) in self.layout(available).into_iter().enumerate() {
            if x < left + width / 2.0 {
                return column_index;
            }
            left += width + gap;
            index = column_index + 1;
        }
        index.saturating_sub(1)
    }

    /// The visible column under `x`.
    pub fn column_at(&self, x: f32, available: f32, gap: f32) -> Option<Column> {
        let mut left = 0.0;
        for (column, width) in self.layout(available) {
            if x < left + width + gap {
                return Some(column);
            }
            left += width + gap;
        }
        self.visible().last().map(|s| s.column)
    }
}

/// Rough on-screen width of `text` at `size` px: wide (CJK) characters count
/// as a full em, others a little over half.
pub fn text_width(text: &str, size: f32) -> f32 {
    text.chars()
        .map(|c| if (c as u32) >= 0x1100 && !c.is_ascii() { 1.0 } else { 0.58 })
        .sum::<f32>()
        * size
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(c: &Columns, available: f32) -> Vec<f32> {
        c.layout(available).into_iter().map(|(_, w)| w.round()).collect()
    }

    #[test]
    fn fit_fills_the_available_width() {
        let c = Columns::default();
        let total: f32 = c.layout(1200.0).iter().map(|(_, w)| w).sum();
        assert!((total - 1200.0).abs() < 0.5, "{total}");
        let fixed = Columns { fit: false, ..Columns::default() };
        assert_eq!(widths(&fixed, 5000.0), [320.0, 120.0, 190.0, 100.0, 100.0, 130.0]);
    }

    #[test]
    fn resizing_with_fit_trades_with_the_neighbour() {
        let mut c = Columns::default();
        let before = widths(&c, 960.0);
        c.resize(Column::Size, 30.0, 960.0);
        let after = widths(&c, 960.0);
        assert_eq!(after[1], before[1] + 30.0);
        assert_eq!(after[2], before[2] - 30.0);
        assert_eq!(after.iter().sum::<f32>().round(), 960.0);
        // Never below the minimum.
        c.resize(Column::Size, -10_000.0, 960.0);
        assert_eq!(widths(&c, 960.0)[1], Column::Size.min_width());
    }

    #[test]
    fn resizing_without_fit_changes_one_column() {
        let mut c = Columns { fit: false, ..Columns::default() };
        c.resize(Column::Speed, 40.0, 0.0);
        assert_eq!(widths(&c, 0.0), [320.0, 120.0, 190.0, 140.0, 100.0, 130.0]);
    }

    #[test]
    fn moving_and_hiding_columns() {
        let mut c = Columns::default();
        c.move_to(Column::Status, 1);
        let order: Vec<_> = c.visible().map(|s| s.column).collect();
        assert_eq!(order[..3], [Column::Name, Column::Status, Column::Size]);
        c.toggle(Column::Size);
        c.toggle(Column::Name);
        let order: Vec<_> = c.visible().map(|s| s.column).collect();
        assert_eq!(
            order,
            [Column::Name, Column::Status, Column::Progress, Column::Speed, Column::TimeLeft]
        );
        c.shift(Column::Name, 1);
        assert_eq!(c.visible().next().unwrap().column, Column::Status);
        c.shift(Column::Status, -1);
        assert_eq!(c.visible().next().unwrap().column, Column::Status, "already first");
    }

    #[test]
    fn hit_testing() {
        let c = Columns { fit: false, ..Columns::default() };
        assert_eq!(c.column_at(10.0, 0.0, 0.0), Some(Column::Name));
        assert_eq!(c.column_at(330.0, 0.0, 0.0), Some(Column::Size));
        assert_eq!(c.drop_index(100.0, 0.0, 0.0), 0);
        assert_eq!(c.drop_index(200.0, 0.0, 0.0), 1);
        assert_eq!(c.drop_index(10_000.0, 0.0, 0.0), 5);
    }

    #[test]
    fn validation_repairs_saved_columns() {
        let broken = Columns {
            items: vec![
                ColumnState { column: Column::Speed, width: f32::NAN, visible: true },
                ColumnState { column: Column::Speed, width: 90.0, visible: true },
                ColumnState { column: Column::Name, width: 3.0, visible: false },
            ],
            fit: false,
        }
        .validated();
        assert_eq!(broken.items.len(), 6);
        assert_eq!(broken.items[0].width, 100.0);
        assert!(broken.items[1].visible && broken.items[1].width >= Column::Name.min_width());
    }

    #[test]
    fn text_width_counts_wide_characters() {
        assert!(text_width("abc", 10.0) < text_width("日本語", 10.0));
    }

    #[test]
    fn round_trips_through_toml() {
        #[derive(Serialize, Deserialize)]
        struct W {
            columns: Columns,
        }
        let c = Columns { fit: false, ..Columns::default() };
        let text = toml::to_string(&W { columns: c.clone() }).unwrap();
        assert_eq!(toml::from_str::<W>(&text).unwrap().columns, c);
    }
}
