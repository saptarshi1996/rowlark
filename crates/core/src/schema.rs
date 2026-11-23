//! The normalized "shape of a database" model.
//!
//! This is what introspection returns. The UI draws the sidebar tree and the
//! Structure tab from these types and never sees Postgres or MySQL specifics.
//! Type names are the server's own text (e.g. `numeric(12,2)`), because that is
//! exactly what a person wants to read.

/// What kind of object a table-like entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
}

/// One entry in the sidebar tree: a table or view, without its details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub name: String,
    pub kind: TableKind,
    /// The server's *estimate* of the row count (cheap to get, not exact).
    /// `None` when the server has no estimate yet (never analyzed).
    pub estimated_rows: Option<i64>,
}

/// One column of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// The server's own type text, e.g. "character varying(10)".
    pub data_type: String,
    pub nullable: bool,
    /// The default expression as the server prints it, e.g. "now()".
    pub default: Option<String>,
    /// An identity column: the server fills it in (so inserts can skip it).
    pub identity: bool,
    /// A generated column: computed by the server, cannot be written to.
    pub generated: bool,
}

/// One index, including the ones behind primary keys and unique constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    /// Column names in index order. Expression indexes list only plain columns;
    /// see `definition` for the full picture.
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// The index method, e.g. "btree" or "gin".
    pub method: String,
    /// The full `CREATE INDEX ...` statement, to show as-is.
    pub definition: String,
}

/// A foreign key from this table to another table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    pub name: String,
    /// Columns in this table, in order.
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    /// Columns in the referenced table, in the same order as `columns`.
    pub referenced_columns: Vec<String>,
    /// What happens on update/delete: "NO ACTION", "RESTRICT", "CASCADE",
    /// "SET NULL" or "SET DEFAULT".
    pub on_update: String,
    pub on_delete: String,
}

/// Everything the Structure tab shows for one table or view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableDetails {
    pub columns: Vec<ColumnInfo>,
    /// Primary key columns in order (empty if there is no primary key).
    /// Editing a row needs this to find it again.
    pub primary_key: Vec<String>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
}
