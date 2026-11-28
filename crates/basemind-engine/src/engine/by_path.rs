//! Rows grouped under the file they are in: `{"<path>": [row, ...], ...}`.
//!
//! A locating answer (a grep hit, a call site) is a line in a file, and a file usually holds several.
//! Written flat, every row repeats its path and the `"path":` key; grouped, a file's path is written
//! once. The rows stay in the order they were found, and the files in the order of their first row.
//!
//! A row is located by its line. Its `column` is written only where the line alone does not place it.
//! A grep hit and a call site decide that over the whole result ([`Located::writes_column`]), so a
//! page boundary never changes a row; any other row writes it when another row of the page is on
//! the same line of the same file.

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

use crate::path::RelPath;

/// A row that belongs to one line of one file.
pub trait Located {
    fn located(&self) -> &RelPath;
    /// 1-based line, `0` when the row has no recorded position.
    fn line(&self) -> u32;
    /// Whether the row writes its column, when the row decided it over the whole result. `None`
    /// leaves it to the page: written when another row of the page is on the same line.
    fn writes_column(&self) -> Option<bool> {
        None
    }
    /// The group a row is written under inside its file, when rows are grouped again there
    /// (`code_grep`: the enclosing symbol, `""` outside every symbol). Its key is written once per
    /// file and group.
    fn group(&self) -> Option<&str> {
        None
    }
    /// Write the row without its path, and with its 1-based column when `column` is set.
    fn serialize_row<S: Serializer>(&self, serializer: S, column: bool) -> Result<S::Ok, S::Error>;
}

/// One row as listed: with its column when its line holds another row.
struct Listed<'a, T> {
    row: &'a T,
    column: bool,
}

impl<T: Located> Serialize for Listed<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.row.serialize_row(serializer, self.column)
    }
}

/// `serialize_with` for a row list: a map from each file to its rows. A path that is not UTF-8 is
/// written lossily, since a JSON key is a string.
pub(crate) fn serialize<S: Serializer, T: Located>(rows: &[T], serializer: S) -> Result<S::Ok, S::Error> {
    let mut on_line: ahash::AHashMap<(&RelPath, u32), usize> = ahash::AHashMap::new();
    for row in rows.iter().filter(|row| row.line() != 0) {
        *on_line.entry((row.located(), row.line())).or_default() += 1;
    }
    let mut groups: Vec<(&RelPath, Vec<Listed<'_, T>>)> = Vec::new();
    let mut slot: ahash::AHashMap<&RelPath, usize> = ahash::AHashMap::new();
    for row in rows {
        let path = row.located();
        let index = *slot.entry(path).or_insert_with(|| {
            groups.push((path, Vec::new()));
            groups.len() - 1
        });
        let column = row
            .writes_column()
            .unwrap_or_else(|| on_line.get(&(path, row.line())).is_some_and(|rows| *rows > 1));
        groups[index].1.push(Listed { row, column });
    }
    let mut map = serializer.serialize_map(Some(groups.len()))?;
    for (path, rows) in &groups {
        map.serialize_entry(&path.to_str_lossy(), rows)?;
    }
    map.end()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Row {
        path: RelPath,
        line: u32,
        column: u32,
    }

    impl Located for Row {
        fn located(&self) -> &RelPath {
            &self.path
        }

        fn line(&self) -> u32 {
            self.line
        }

        fn serialize_row<S: Serializer>(&self, serializer: S, column: bool) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(None)?;
            map.serialize_entry("line", &self.line)?;
            if column {
                map.serialize_entry("column", &self.column)?;
            }
            map.end()
        }
    }

    #[derive(Serialize)]
    struct Page {
        #[serde(serialize_with = "serialize")]
        hits: Vec<Row>,
    }

    fn row(path: &str, line: u32, column: u32) -> Row {
        Row {
            path: path.into(),
            line,
            column,
        }
    }

    #[test]
    fn rows_group_under_their_file_in_the_order_found() {
        let page = Page {
            hits: vec![row("b.rs", 3, 1), row("a.rs", 1, 1), row("b.rs", 9, 1)],
        };
        assert_eq!(
            serde_json::to_string(&page).unwrap(),
            r#"{"hits":{"b.rs":[{"line":3},{"line":9}],"a.rs":[{"line":1}]}}"#
        );
        let empty = Page { hits: Vec::new() };
        assert_eq!(serde_json::to_string(&empty).unwrap(), r#"{"hits":{}}"#);
    }

    #[test]
    fn rows_on_one_line_write_their_columns() {
        let page = Page {
            hits: vec![
                row("a.rs", 2, 5),
                row("a.rs", 2, 14),
                row("a.rs", 3, 5),
                row("b.rs", 2, 5),
            ],
        };
        assert_eq!(
            serde_json::to_string(&page).unwrap(),
            r#"{"hits":{"a.rs":[{"line":2,"column":5},{"line":2,"column":14},{"line":3}],"b.rs":[{"line":2}]}}"#
        );
    }

    #[test]
    fn a_budgeted_page_is_written_within_its_budget_when_every_row_is_in_its_own_file() {
        let rows: Vec<Row> = (0..200)
            .map(|i| row(&format!("src/some/deeply/nested/module_{i}/file.rs"), 3, 1))
            .collect();
        let budget = 100;
        let kept = crate::engine::budget::apply_budget_by_path(rows, Some(budget));
        assert!(kept.budgeted);
        assert!(kept.items.len() > 1, "more than the one row a page always keeps");
        let written = serde_json::to_string(&Page { hits: kept.items }).unwrap();
        let hits = written.strip_prefix(r#"{"hits":"#).unwrap();
        assert!(
            hits.len() as u64 <= u64::from(budget) * 4 + 4,
            "{} bytes written for a {budget}-token budget",
            hits.len()
        );
    }
}
