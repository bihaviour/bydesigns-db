//! Table catalog: schemas the planner binds names and types against.

use crate::value::ColumnType;

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    pub primary_key: bool,
    pub not_null: bool,
    /// A single-column `UNIQUE` constraint (stage 6D). Enforced like the PK.
    pub unique: bool,
    /// `AUTOINCREMENT` / `SERIAL` (stage 6D): the engine fills an omitted/NULL
    /// value with a monotonic per-table counter.
    pub autoincrement: bool,
    /// A `DEFAULT <expr>` clause, stored as the original SQL text and re-parsed
    /// when an insert omits the column (stage 6D).
    pub default_sql: Option<String>,
}

/// A foreign-key constraint: one or more local columns referencing the same
/// number of columns in `foreign_table`. Carried so the pgwire server can
/// reflect relationships into PostgREST's schema cache (resource embedding);
/// the engine itself does not enforce referential integrity in this phase.
#[derive(Clone, Debug)]
pub struct ForeignKey {
    /// Constraint name (synthesized as `<table>_<col>_fkey` when not declared).
    pub name: String,
    /// Local columns, in declaration order.
    pub columns: Vec<String>,
    /// Referenced table.
    pub foreign_table: String,
    /// Referenced columns, parallel to `columns`.
    pub foreign_columns: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<Column>,
    pub foreign_keys: Vec<ForeignKey>,
    /// `CHECK (expr)` predicates (column- and table-level), as SQL text — parsed
    /// and evaluated against each inserted/updated row (stage 6D).
    pub checks: Vec<String>,
    /// Table-level / composite `UNIQUE (cols)` constraints (stage 6D). Single
    /// inline `UNIQUE` lives on the column instead (`Column::unique`).
    pub uniques: Vec<Vec<String>>,
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The first primary-key column, if any (the historical single-column PK
    /// accessor; prefer [`TableSchema::primary_key_indices`] for composite keys).
    pub fn primary_key_index(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.primary_key)
    }

    /// All primary-key column indices, in declaration order (may be composite).
    pub fn primary_key_indices(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.primary_key)
            .map(|(i, _)| i)
            .collect()
    }

    /// Every unique key set to enforce, as column-index lists: the primary key
    /// first (if any), then single-column `UNIQUE` columns, then table-level
    /// `UNIQUE (cols)` constraints.
    pub fn unique_sets(&self) -> Vec<Vec<usize>> {
        let mut sets = Vec::new();
        let pk = self.primary_key_indices();
        if !pk.is_empty() {
            sets.push(pk);
        }
        for (i, c) in self.columns.iter().enumerate() {
            if c.unique {
                sets.push(vec![i]);
            }
        }
        for u in &self.uniques {
            let idxs: Vec<usize> = u.iter().filter_map(|n| self.column_index(n)).collect();
            if idxs.len() == u.len() {
                sets.push(idxs);
            }
        }
        sets
    }
}
