//! An entity-relationship diagram of one schema: its tables as boxes, its
//! foreign keys as the lines between them.
//!
//! Read out of [`Connection::structure`], the same call the Structure toggle
//! makes, one relation at a time. Nothing here writes SQL or reaches below
//! `src/db/`: the diagram is a reading of definitions the engines already
//! answer for, so every engine that names its foreign keys draws lines and the
//! two that do not (Snowflake, MongoDB) draw boxes alone.
//!
//! The model is in diagram units, which are pixels at 100% zoom. The view in
//! `workspace::diagram` scales and pans it; nothing in here knows a window.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::db::{Connection, DbError, RelationKind, Structure};

pub(crate) const BOX_WIDTH: f32 = 240.0;
pub(crate) const HEADER_HEIGHT: f32 = 30.0;
pub(crate) const ROW_HEIGHT: f32 = 22.0;
/// Past this, a box ends in a "… N more" row: a 200-column table drawn whole
/// would be a wall the lines have to go round.
const MAX_ROWS: usize = 24;
const GAP_X: f32 = 120.0;
const GAP_Y: f32 = 36.0;
/// How tall a column of boxes grows before the rest of its rank wraps into the
/// next one, so a hub with sixty children does not become a ten-screen tower.
const MAX_COLUMN_HEIGHT: f32 = 1800.0;
/// How many relations one diagram reads. Each is a round trip; past this many
/// the picture stops being readable anyway.
pub(crate) const MAX_TABLES: usize = 150;

#[derive(Clone, Debug)]
pub(crate) struct Diagram {
    pub(crate) tables: Vec<Table>,
    pub(crate) links: Vec<Link>,
    /// Relations of the schema not drawn because of [`MAX_TABLES`].
    pub(crate) left_out: usize,
    /// Relations whose definition could not be read, drawn as nothing.
    pub(crate) unreadable: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Table {
    pub(crate) name: String,
    pub(crate) kind: RelationKind,
    pub(crate) fields: Vec<Field>,
    pub(crate) x: f32,
    pub(crate) y: f32,
}

#[derive(Clone, Debug)]
pub(crate) struct Field {
    pub(crate) name: String,
    pub(crate) data_type: String,
    pub(crate) primary: bool,
    pub(crate) foreign: bool,
    pub(crate) nullable: bool,
}

/// One column of `from` holding values of one column of `to`. A composite key
/// is several of these, as it is in [`Structure::foreign_keys`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Link {
    pub(crate) from: usize,
    pub(crate) from_row: usize,
    pub(crate) to: usize,
    pub(crate) to_row: usize,
    /// The referencing column is unique on its own, so at most one row points
    /// at each referenced row: 1:1 rather than 1:N.
    pub(crate) one_to_one: bool,
    /// The referencing column takes `NULL`, so a row need not point anywhere.
    pub(crate) optional: bool,
}

impl Table {
    pub(crate) fn is_view(&self) -> bool {
        matches!(
            self.kind,
            RelationKind::View | RelationKind::MaterializedView
        )
    }

    /// The columns the box draws; the rest are counted in its last row.
    pub(crate) fn shown(&self) -> &[Field] {
        &self.fields[..self.fields.len().min(MAX_ROWS)]
    }

    /// Columns past [`MAX_ROWS`], every one still in `fields` for an export.
    pub(crate) fn hidden(&self) -> usize {
        self.fields.len().saturating_sub(MAX_ROWS)
    }

    fn rows(&self) -> usize {
        (self.shown().len() + usize::from(self.hidden() > 0)).max(1)
    }

    pub(crate) fn height(&self) -> f32 {
        HEADER_HEIGHT + ROW_HEIGHT * self.rows() as f32
    }

    /// The vertical middle of a row, in diagram units. A row hidden past
    /// [`MAX_ROWS`] answers with the "… more" row, which is where it went.
    pub(crate) fn row_middle(&self, row: usize) -> f32 {
        let row = row.min(self.rows() - 1);
        self.y + HEADER_HEIGHT + ROW_HEIGHT * (row as f32 + 0.5)
    }
}

impl Diagram {
    /// The diagram as a Mermaid `erDiagram`, for a README or a note: every
    /// column, not only the ones a box has room for, and the same crow's-foot
    /// ends the canvas draws.
    ///
    /// Mermaid's grammar takes a narrower name than SQL does, so names and
    /// types are spelled down to what it reads ([`mermaid_word`]); the label
    /// on each relationship keeps the real column names, quoted.
    pub(crate) fn mermaid(&self) -> String {
        let mut out = String::from("erDiagram\n");
        for table in &self.tables {
            out.push_str(&format!("    {} {{\n", mermaid_word(&table.name)));
            for field in &table.fields {
                let keys: Vec<&str> = [(field.primary, "PK"), (field.foreign, "FK")]
                    .into_iter()
                    .filter_map(|(is, key)| is.then_some(key))
                    .collect();
                let data_type = match field.data_type.trim() {
                    "" => "unknown".to_string(),
                    data_type => mermaid_word(data_type),
                };
                out.push_str(&format!(
                    "        {} {}{}\n",
                    data_type,
                    mermaid_word(&field.name),
                    match keys.is_empty() {
                        true => String::new(),
                        false => format!(" {}", keys.join(",")),
                    }
                ));
            }
            out.push_str("    }\n");
        }
        for link in &self.links {
            let (from, to) = (&self.tables[link.from], &self.tables[link.to]);
            // Referenced on the left: exactly one, or zero or one when the key
            // takes NULL. Referencing on the right: zero or one when the key
            // is unique by itself, else zero or more.
            let one = if link.optional { "|o" } else { "||" };
            let many = if link.one_to_one { "o|" } else { "o{" };
            out.push_str(&format!(
                "    {} {one}--{many} {} : \"{} → {}\"\n",
                mermaid_word(&to.name),
                mermaid_word(&from.name),
                from.fields[link.from_row].name.replace('"', "'"),
                to.fields[link.to_row].name.replace('"', "'"),
            ));
        }
        out
    }
}

/// A name or a type as Mermaid's ER grammar will read it: letters, digits,
/// `_` and `-`, starting with a letter or `_`. `character varying(255)` comes
/// out `character_varying_255`, which is still the type a reader recognises.
fn mermaid_word(text: &str) -> String {
    let mut word: String = text
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                true => c,
                false => '_',
            },
        )
        .collect();
    while word.contains("__") {
        word = word.replace("__", "_");
    }
    let word = word.trim_matches('_').to_string();
    match word.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => word,
        _ => format!("_{word}"),
    }
}

/// Read every named relation's definition and lay the result out. Blocking:
/// run it on the background executor. Once `cancelled` is set it reads no
/// further, and what it hands back is for nobody.
///
/// The whole schema in one read where the engine has one, else a relation at
/// a time. Read whole, a schema past [`MAX_TABLES`] keeps its most joined
/// tables, which are the ones the picture is about; read one at a time,
/// nothing is known about the keys before the cut, so it keeps the first.
pub(crate) fn load(
    connection: &Connection,
    schema: &str,
    names: Vec<(String, RelationKind)>,
    cancelled: &AtomicBool,
) -> Result<Diagram, DbError> {
    let whole = match connection.schema_structures(schema) {
        Some(Ok(structures)) => Some(structures),
        // A side session the server would not open (a connection limit, a
        // role allowed one session) still leaves the user's own to read on.
        Some(Err(_)) | None => None,
    };
    let (read, left_out, unreadable, first_error) = match whole {
        Some(mut structures) => {
            let names = most_joined(schema, names, &structures);
            let left_out = names.len().saturating_sub(MAX_TABLES);
            let mut read = Vec::new();
            let mut unreadable = 0;
            for (name, kind) in names.into_iter().take(MAX_TABLES) {
                match structures.remove(&name) {
                    Some(structure) => read.push((name, kind, structure)),
                    None => unreadable += 1,
                }
            }
            (read, left_out, unreadable, None)
        }
        None => {
            let left_out = names.len().saturating_sub(MAX_TABLES);
            let mut read = Vec::new();
            let mut first_error = None;
            let mut unreadable = 0;
            for (name, kind) in names.into_iter().take(MAX_TABLES) {
                if cancelled.load(Ordering::Relaxed) {
                    break;
                }
                match connection.structure(schema, &name) {
                    Ok(structure) => read.push((name, kind, structure)),
                    Err(error) => {
                        unreadable += 1;
                        first_error.get_or_insert(error);
                    }
                }
            }
            (read, left_out, unreadable, first_error)
        }
    };
    // Not one definition came back: that is the connection failing, not a few
    // relations being odd, and an empty canvas would hide it.
    if read.is_empty()
        && let Some(error) = first_error
    {
        return Err(error);
    }
    let mut diagram = assemble(schema, read);
    diagram.left_out = left_out;
    diagram.unreadable = unreadable;
    lay_out(&mut diagram);
    Ok(diagram)
}

/// `names` with the most joined first: by how many keys, in or out, tie each
/// to another relation of the schema, ties kept in the order they came.
fn most_joined(
    schema: &str,
    mut names: Vec<(String, RelationKind)>,
    structures: &HashMap<String, Structure>,
) -> Vec<(String, RelationKind)> {
    if names.len() <= MAX_TABLES {
        return names;
    }
    let mut degree: HashMap<&str, usize> = HashMap::new();
    for (name, structure) in structures {
        for key in &structure.foreign_keys {
            if key.referenced_schema == schema && key.referenced_table != *name {
                *degree.entry(name.as_str()).or_default() += 1;
                *degree.entry(key.referenced_table.as_str()).or_default() += 1;
            }
        }
    }
    names.sort_by_key(|(name, _)| {
        std::cmp::Reverse(degree.get(name.as_str()).copied().unwrap_or(0))
    });
    names
}

/// The boxes and lines, before anything has a position.
fn assemble(schema: &str, read: Vec<(String, RelationKind, Structure)>) -> Diagram {
    let index: HashMap<&str, usize> = read
        .iter()
        .enumerate()
        .map(|(position, (name, _, _))| (name.as_str(), position))
        .collect();

    let mut links = Vec::new();
    for (from, (_, _, structure)) in read.iter().enumerate() {
        for key in &structure.foreign_keys {
            // A key into another schema has no box here to land on.
            if key.referenced_schema != schema {
                continue;
            }
            // Exactly as spelled, else in any case: SQLite takes
            // `REFERENCES Users(ID)` for `users(id)` and reports the key the
            // way it was written.
            let Some(to) = index
                .get(key.referenced_table.as_str())
                .copied()
                .or_else(|| {
                    read.iter()
                        .position(|(name, _, _)| name.eq_ignore_ascii_case(&key.referenced_table))
                })
            else {
                continue;
            };
            let row_of = |structure: &Structure, column: &str| {
                let columns = &structure.columns;
                columns.iter().position(|c| c.name == column).or_else(|| {
                    columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(column))
                })
            };
            let (Some(from_row), Some(to_row)) = (
                row_of(structure, &key.column),
                row_of(&read[to].2, &key.referenced_column),
            ) else {
                continue;
            };
            links.push(Link {
                from,
                from_row,
                to,
                to_row,
                one_to_one: unique_alone(structure, &key.column),
                optional: structure.columns[from_row].nullable,
            });
        }
    }

    let tables = read
        .into_iter()
        .map(|(name, kind, structure)| {
            let primary = structure.primary_key();
            let fields: Vec<Field> = structure
                .columns
                .iter()
                .map(|column| Field {
                    name: column.name.clone(),
                    data_type: column.data_type.clone(),
                    primary: primary.contains(&column.name),
                    foreign: structure
                        .foreign_keys
                        .iter()
                        .any(|key| key.column == column.name),
                    nullable: column.nullable,
                })
                .collect();
            Table {
                name,
                kind,
                fields,
                x: 0.0,
                y: 0.0,
            }
        })
        .collect();

    Diagram {
        tables,
        links,
        left_out: 0,
        unreadable: 0,
    }
}

/// Whether `column` is unique by itself: the whole primary key, or the one
/// column of a unique constraint or index. Read off the rendered definitions,
/// which spell it `UNIQUE (col)`, `UNIQUE NONCLUSTERED INDEX (col)` or
/// `CREATE UNIQUE INDEX … (col)`, quoted or not depending on the engine.
///
/// Only the key list counts: not an `INCLUDE (…)` the index carries along,
/// and not a partial index's `WHERE`, which is unique over some rows only.
fn unique_alone(structure: &Structure, column: &str) -> bool {
    if structure.primary_key() == [column] {
        return true;
    }
    structure
        .constraints
        .iter()
        .chain(&structure.indexes)
        .filter_map(|definition| unique_key(&definition.definition))
        .any(|key| key == column)
}

/// The one column a unique definition keys on, or `None` when it is not
/// unique, keys on several columns or an expression, or is partial.
fn unique_key(definition: &str) -> Option<String> {
    let text = definition.trim().trim_end_matches(';').trim_end();
    let upper = text.to_ascii_uppercase();
    // Past the index's name and table, so neither can pass for the key list.
    let from = match () {
        _ if upper.starts_with("UNIQUE") => 0,
        _ if upper.starts_with("CREATE UNIQUE ") => upper.find(" ON ")? + 4,
        _ => return None,
    };
    let open = from + text[from..].find('(')?;
    let mut depth = 0;
    let close = text[open..].char_indices().find_map(|(at, c)| {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        (depth == 0).then_some(open + at)
    })?;
    if upper[close..].contains(" WHERE ") {
        return None;
    }
    let inner = text[open + 1..close].trim();
    if inner.contains([',', '(']) {
        return None;
    }
    // The name, without the quotes it may need and the order or collation
    // after it, which do not change what is unique.
    let name = match inner.chars().next()? {
        quote @ ('"' | '`' | '[') => {
            let end = if quote == '[' { ']' } else { quote };
            inner[1..].split(end).next()?.to_string()
        }
        _ => inner.split_whitespace().next()?.to_string(),
    };
    Some(name)
}

/// Layered placement: a table sits one rank right of every table it points
/// at, so every line runs the same way, from a referencing box back to the
/// one it references. Within a rank, boxes are ordered by the mean position
/// of what they point at (the barycenter heuristic), which untangles most of
/// the crossings for none of the cost of minimising them.
///
/// Tables with no line to anything go last, in name order, out of the way,
/// and views after them.
fn lay_out(diagram: &mut Diagram) {
    let count = diagram.tables.len();
    if count == 0 {
        return;
    }
    let edges: Vec<(usize, usize)> = diagram
        .links
        .iter()
        .filter(|link| link.from != link.to)
        .map(|link| (link.from, link.to))
        .collect();

    let mut connected = vec![false; count];
    for &(from, to) in &edges {
        connected[from] = true;
        connected[to] = true;
    }

    // Longest path from a table nothing else is pointed at by. Bounded
    // passes rather than a topological sort: a cycle of keys (a parent
    // pointing at its own latest child) only stops the ranks growing, it
    // does not stop the loop.
    let mut rank = vec![0usize; count];
    for _ in 0..count.min(64) {
        let mut changed = false;
        for &(from, to) in &edges {
            if rank[from] < rank[to] + 1 && rank[to] + 1 < count {
                rank[from] = rank[to] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let incoming = |table: usize| edges.iter().filter(|&&(_, to)| to == table).count();
    let top_rank = (0..count).filter(|&t| connected[t]).map(|t| rank[t]).max();

    // Each rank's tables in order, ordered against the placed ranks before it.
    let mut order: Vec<Vec<usize>> = Vec::new();
    let mut slot = vec![0f32; count];
    for r in 0..=top_rank.unwrap_or(0) {
        let mut members: Vec<usize> = (0..count)
            .filter(|&t| connected[t] && rank[t] == r)
            .collect();
        if r == 0 {
            // The hubs first: the most-referenced tables are where the eye
            // starts, so they go at the top of the first column.
            members.sort_by(|&a, &b| {
                incoming(b)
                    .cmp(&incoming(a))
                    .then_with(|| diagram.tables[a].name.cmp(&diagram.tables[b].name))
            });
        } else {
            let barycenter = |t: usize| {
                let parents: Vec<f32> = edges
                    .iter()
                    .filter(|&&(from, to)| from == t && rank[to] < r)
                    .map(|&(_, to)| slot[to])
                    .collect();
                match parents.is_empty() {
                    true => f32::MAX,
                    false => parents.iter().sum::<f32>() / parents.len() as f32,
                }
            };
            members.sort_by(|&a, &b| {
                barycenter(a)
                    .total_cmp(&barycenter(b))
                    .then_with(|| diagram.tables[a].name.cmp(&diagram.tables[b].name))
            });
        }
        for (position, &t) in members.iter().enumerate() {
            slot[t] = position as f32;
        }
        if !members.is_empty() {
            order.push(members);
        }
    }

    let mut loose: Vec<usize> = (0..count).filter(|&t| !connected[t]).collect();
    // Views last, after the loose tables, so they gather in their own columns.
    loose.sort_by(|&a, &b| {
        let (a, b) = (&diagram.tables[a], &diagram.tables[b]);
        (a.is_view(), &a.name).cmp(&(b.is_view(), &b.name))
    });
    if !loose.is_empty() {
        order.push(loose);
    }

    // Columns, wrapping a tall rank into the next one over.
    let mut x = 0.0;
    for members in order {
        let mut y = 0.0;
        for t in members {
            let height = diagram.tables[t].height();
            if y > 0.0 && y + height > MAX_COLUMN_HEIGHT {
                x += BOX_WIDTH + GAP_X;
                y = 0.0;
            }
            diagram.tables[t].x = x;
            diagram.tables[t].y = y;
            y += height + GAP_Y;
        }
        x += BOX_WIDTH + GAP_X;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NamedDefinition;

    fn with(constraints: &[&str], indexes: &[&str]) -> Structure {
        let named = |definitions: &[&str]| {
            definitions
                .iter()
                .enumerate()
                .map(|(n, definition)| NamedDefinition {
                    name: format!("d{n}"),
                    definition: definition.to_string(),
                })
                .collect()
        };
        Structure {
            constraints: named(constraints),
            indexes: named(indexes),
            ..Structure::default()
        }
    }

    fn relation(columns: &[&str], keys: &[(&str, &str, &str)]) -> Structure {
        Structure {
            columns: columns
                .iter()
                .map(|name| crate::db::ColumnDefinition {
                    name: name.to_string(),
                    data_type: "integer".into(),
                    nullable: false,
                    default: None,
                })
                .collect(),
            foreign_keys: keys
                .iter()
                .map(|&(column, table, referenced)| crate::db::ForeignKey {
                    column: column.into(),
                    referenced_schema: "main".into(),
                    referenced_table: table.into(),
                    referenced_column: referenced.into(),
                })
                .collect(),
            ..Structure::default()
        }
    }

    #[test]
    fn a_key_spelled_in_another_case_still_draws_its_line() {
        // SQLite takes `REFERENCES Users(ID)` for `users(id)` and hands the
        // key back spelled the way it was written.
        let diagram = assemble(
            "main",
            vec![
                ("users".into(), RelationKind::Table, relation(&["id"], &[])),
                (
                    "orders".into(),
                    RelationKind::Table,
                    relation(&["id", "user_id"], &[("user_id", "Users", "ID")]),
                ),
            ],
        );
        assert_eq!(diagram.links.len(), 1);
        assert_eq!((diagram.links[0].from, diagram.links[0].to), (1, 0));
        assert_eq!(diagram.links[0].to_row, 0);
    }

    #[test]
    fn an_exact_spelling_wins_over_one_in_another_case() {
        let diagram = assemble(
            "main",
            vec![
                ("Users".into(), RelationKind::Table, relation(&["id"], &[])),
                ("users".into(), RelationKind::Table, relation(&["id"], &[])),
                (
                    "orders".into(),
                    RelationKind::Table,
                    relation(&["user_id"], &[("user_id", "users", "id")]),
                ),
            ],
        );
        assert_eq!(diagram.links[0].to, 1);
    }

    #[test]
    fn a_unique_constraint_or_index_on_the_column_alone_is_one_to_one() {
        for structure in [
            with(&["UNIQUE (order_id)"], &[]),
            with(&["UNIQUE NULLS NOT DISTINCT (order_id)"], &[]),
            with(&["UNIQUE (\"order_id\")"], &[]),
            // Postgres, SQLite, MySQL and SQL Server spell an index each their
            // own way.
            with(
                &[],
                &["CREATE UNIQUE INDEX p ON public.payments USING btree (order_id)"],
            ),
            with(&[], &["CREATE UNIQUE INDEX p ON payments(order_id DESC)"]),
            with(&[], &["UNIQUE INDEX (order_id)"]),
            with(&[], &["UNIQUE NONCLUSTERED INDEX ([order_id])"]),
        ] {
            assert!(unique_alone(&structure, "order_id"), "{structure:?}");
        }
    }

    #[test]
    fn a_key_that_is_not_unique_by_itself_is_one_to_many() {
        for structure in [
            with(&["UNIQUE (order_id, line)"], &[]),
            with(&["PRIMARY KEY (order_id, line)"], &[]),
            // "unique" in the name of an index that is not.
            with(
                &[],
                &["CREATE INDEX payments_unique_lookup ON payments(order_id)"],
            ),
            with(&[], &["INDEX (order_id)"]),
            // The column is only carried along, or only unique in part.
            with(
                &[],
                &["CREATE UNIQUE INDEX p ON public.payments USING btree (id) INCLUDE (order_id)"],
            ),
            with(
                &[],
                &["UNIQUE NONCLUSTERED INDEX ([id]) INCLUDE ([order_id])"],
            ),
            with(
                &[],
                &["CREATE UNIQUE INDEX p ON payments(order_id) WHERE amount > 0"],
            ),
            with(
                &[],
                &["UNIQUE NONCLUSTERED INDEX ([order_id]) WHERE ([amount]>(0))"],
            ),
            // Unique over an expression of the column, not the column.
            with(
                &[],
                &["CREATE UNIQUE INDEX p ON payments USING btree (lower(order_id))"],
            ),
        ] {
            assert!(!unique_alone(&structure, "order_id"), "{structure:?}");
        }
    }
}
