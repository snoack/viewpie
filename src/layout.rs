//! The wall's shape, as a picture.
//!
//! A layout is written as a grid of slot names, one row per line, and a name
//! repeated over a block of cells spans them:
//!
//! ```text
//!   a a b
//!   a a c
//!   d e .
//! ```
//!
//! Which is a 2x2 viewport, four single viewports, and one cell left as background.
//! Nothing says where anything is or how big it is, because the picture
//! already does, and it cannot express a wall with a hole in it or two
//! viewports claiming the same cell, because a cell holds one name.
//!
//! Without a picture the viewports fill the smallest square grid that holds
//! them, in order, one cell each.

/// Where one viewport sits in the grid, in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub col: u32,
    pub row: u32,
    /// Cells spanned in each direction. Spans are square, so one number.
    pub span: u32,
}

/// A parsed layout: the grid's size, and where each named slot sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub size: u32,
    pub slots: Vec<(String, Cell)>,
}

impl Layout {
    /// Read a layout picture, or say what is wrong with it.
    ///
    /// The errors name the slot rather than a line and column: a wall is read
    /// as a picture, and "slot 'b' spans 1x2" is what the author can see.
    pub fn parse(text: &str) -> Result<Layout, String> {
        let rows: Vec<Vec<&str>> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| line.split_whitespace().collect())
            .collect();

        if rows.is_empty() {
            return Err("the layout is empty".into());
        }

        let width = rows[0].len();
        for (n, row) in rows.iter().enumerate() {
            if row.len() != width {
                return Err(format!(
                    "every row must have the same number of cells: row {} has {}, \
                     row 1 has {width}",
                    n + 1,
                    row.len()
                ));
            }
        }
        if rows.len() != width {
            return Err(format!(
                "the layout must be square: it is {width} wide and {} tall",
                rows.len()
            ));
        }

        // Every cell a name occupies, in the order they were written.
        let mut names: Vec<&str> = Vec::new();
        for row in &rows {
            for &cell in row {
                if cell != "." && !names.contains(&cell) {
                    names.push(cell);
                }
            }
        }

        let mut slots = Vec::new();
        for name in names {
            slots.push((name.to_string(), block(&rows, name)?));
        }

        Ok(Layout {
            size: width as u32,
            slots,
        })
    }

    /// The layout a list of viewports gets when none was written: the
    /// smallest square grid that holds them, filled in order.
    pub fn inferred(count: usize) -> Layout {
        // The side of the smallest square that holds them, counted rather
        // than square-rooted: an integer answer to an integer question, and
        // one that is at least 1 without having to be clamped afterwards.
        let size = (1u32..).find(|n| (n * n) as usize >= count).unwrap_or(1);
        let slots = (0..count)
            .map(|i| {
                let i = i as u32;
                (
                    i.to_string(),
                    Cell {
                        col: i % size,
                        row: i / size,
                        span: 1,
                    },
                )
            })
            .collect();
        Layout { size, slots }
    }
}

/// Where a name sits, having checked it is a filled square block.
fn block(rows: &[Vec<&str>], name: &str) -> Result<Cell, String> {
    let mut min_col = usize::MAX;
    let mut min_row = usize::MAX;
    let mut max_col = 0;
    let mut max_row = 0;
    let mut count = 0;

    for (r, row) in rows.iter().enumerate() {
        for (c, &cell) in row.iter().enumerate() {
            if cell == name {
                min_col = min_col.min(c);
                min_row = min_row.min(r);
                max_col = max_col.max(c);
                max_row = max_row.max(r);
                count += 1;
            }
        }
    }

    let w = max_col - min_col + 1;
    let h = max_row - min_row + 1;

    if w != h {
        return Err(format!(
            "slot '{name}' covers {w}x{h} cells; a span must be square"
        ));
    }
    if count != w * h {
        // The name appears in two places, or its block has a hole: either way
        // the cells it covers are not one solid square.
        return Err(format!(
            "slot '{name}' is not one solid block: it covers {count} cells \
             inside a {w}x{h} square"
        ));
    }

    Ok(Cell {
        col: min_col as u32,
        row: min_row as u32,
        span: w as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(layout: &Layout) -> Vec<(&str, u32, u32, u32)> {
        layout
            .slots
            .iter()
            .map(|(n, c)| (n.as_str(), c.col, c.row, c.span))
            .collect()
    }

    #[test]
    fn a_picture_places_and_sizes_every_slot() {
        let layout = Layout::parse("a a b\na a c\nd e .").unwrap();
        assert_eq!(layout.size, 3);
        assert_eq!(
            slots(&layout),
            [
                ("a", 0, 0, 2),
                ("b", 2, 0, 1),
                ("c", 2, 1, 1),
                ("d", 0, 2, 1),
                ("e", 1, 2, 1),
            ]
        );
    }

    #[test]
    fn a_dot_leaves_a_cell_as_background() {
        let layout = Layout::parse(". a\nb .").unwrap();
        assert_eq!(slots(&layout), [("a", 1, 0, 1), ("b", 0, 1, 1)]);
    }

    #[test]
    fn an_oblong_span_is_refused() {
        let err = Layout::parse("a b b\nc d d\ne f g").unwrap_err();
        assert!(err.contains("'b' covers 2x1"), "{err}");
    }

    #[test]
    fn a_split_slot_is_refused() {
        // Opposite corners of a square: the bounding box is square, so only
        // counting the cells inside it catches this.
        let err = Layout::parse("a b\nc a").unwrap_err();
        assert!(err.contains("not one solid block"), "{err}");
    }

    #[test]
    fn a_span_with_a_hole_is_refused() {
        let err = Layout::parse("a a a\na b a\na a a").unwrap_err();
        assert!(err.contains("not one solid block"), "{err}");
    }

    #[test]
    fn a_ragged_picture_is_refused() {
        let err = Layout::parse("a b\nc d e").unwrap_err();
        assert!(err.contains("same number of cells"), "{err}");
    }

    #[test]
    fn a_non_square_grid_is_refused() {
        let err = Layout::parse("a b c\nd e f").unwrap_err();
        assert!(err.contains("must be square"), "{err}");
    }

    #[test]
    fn nine_viewports_infer_a_three_by_three_grid() {
        let layout = Layout::inferred(9);
        assert_eq!(layout.size, 3);
        assert_eq!(layout.slots.len(), 9);
        assert_eq!(
            layout.slots[8].1,
            Cell {
                col: 2,
                row: 2,
                span: 1
            }
        );
    }

    #[test]
    fn five_viewports_infer_a_grid_with_room_to_spare() {
        // Not a 3x2: the cells have to stay square, so the grid does too, and
        // the last four cells are simply background.
        let layout = Layout::inferred(5);
        assert_eq!(layout.size, 3);
        assert_eq!(
            layout.slots[4].1,
            Cell {
                col: 1,
                row: 1,
                span: 1
            }
        );
    }
}
