//! Table catalog: schemas the planner binds names and types against.

use crate::value::ColumnType;

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    pub primary_key: bool,
    pub not_null: bool,
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
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn primary_key_index(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.primary_key)
    }
}
