//! Reflect the engine catalog into the result PostgREST's schema-cache query
//! expects (issue #27). PostgREST/hasql decodes results in **binary**, so the
//! array and composite columns are hand-encoded in the Postgres binary wire
//! format here; scalar text/bool cells ride the normal value path.
//!
//! The tables query returns one row per table:
//!   table_schema, table_name, table_description, is_view,
//!   insertable, updatable, deletable, pk_cols text[], columns record[]
//! where each `columns` element is the 8-field composite
//!   (column_name, description, is_nullable, data_type, nominal_data_type,
//!    character_maximum_length, column_default, enum_labels text[]).

use crate::introspect::{Canned, ReflectKind};
use engine::{CatalogTable, Value};
use std::collections::HashSet;

// Postgres type OIDs used in the reflected RowDescription.
const OID_BOOL: i32 = 16;
const OID_INT4: i32 = 23;
const OID_TEXT: i32 = 25;
const OID_RECORD: i32 = 2249;
const OID_TEXT_ARR: i32 = 1009;
const OID_RECORD_ARR: i32 = 2287;

/// The advertised schema name (the engine has a single, flat namespace).
const SCHEMA: &str = "public";

/// Build the canned result for a schema-cache reflection query from the live
/// catalog. Cells that are Postgres arrays/composites are pre-encoded binary
/// carried as [`Value::Blob`]; the explicit `oids` make the RowDescription match.
pub fn reflect(kind: ReflectKind, catalog: &[CatalogTable]) -> Canned {
    match kind {
        ReflectKind::Tables => tables(catalog),
        ReflectKind::Relationships => relationships(catalog),
    }
}

fn tables(catalog: &[CatalogTable]) -> Canned {
    let columns = [
        "table_schema",
        "table_name",
        "table_description",
        "is_view",
        "insertable",
        "updatable",
        "deletable",
        "pk_cols",
        "columns",
    ]
    .iter()
    .map(|c| c.to_string())
    .collect();
    let oids = vec![
        OID_TEXT,
        OID_TEXT,
        OID_TEXT,
        OID_BOOL,
        OID_BOOL,
        OID_BOOL,
        OID_BOOL,
        OID_TEXT_ARR,
        OID_RECORD_ARR,
    ];

    let rows = catalog
        .iter()
        .map(|t| {
            let pk: Vec<&str> = t
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.as_str())
                .collect();
            let col_records: Vec<Vec<u8>> = t.columns.iter().map(column_composite).collect();
            vec![
                Value::Text(SCHEMA.to_string()),
                Value::Text(t.name.clone()),
                Value::Null,                             // table_description
                Value::Blob(bin_bool(false)),            // is_view
                Value::Blob(bin_bool(true)),             // insertable
                Value::Blob(bin_bool(true)),             // updatable
                Value::Blob(bin_bool(true)),             // deletable
                Value::Blob(text_array(&pk)),            // pk_cols
                Value::Blob(record_array(&col_records)), // columns
            ]
        })
        .collect();

    Canned::Rows {
        columns,
        oids,
        rows,
        tag: format!("SELECT {}", catalog.len()),
    }
}

/// Build the FK-relationship result PostgREST turns into embeddings. One row per
/// foreign key, with `cols_and_fcols` the `record[]` of (local, foreign) column
/// pairs PostgREST decodes (`array_agg(row(col.attname, refs.attname))`).
fn relationships(catalog: &[CatalogTable]) -> Canned {
    let columns = [
        "table_schema",
        "table_name",
        "foreign_table_schema",
        "foreign_table_name",
        "is_self",
        "constraint_name",
        "cols_and_fcols",
        "one_to_one",
    ]
    .iter()
    .map(|c| c.to_string())
    .collect();
    let oids = vec![
        OID_TEXT,       // table_schema
        OID_TEXT,       // table_name
        OID_TEXT,       // foreign_table_schema
        OID_TEXT,       // foreign_table_name
        OID_BOOL,       // is_self
        OID_TEXT,       // constraint_name
        OID_RECORD_ARR, // cols_and_fcols
        OID_BOOL,       // one_to_one
    ];

    let mut rows = Vec::new();
    for t in catalog {
        // The table's primary key — a FK whose columns match it is one-to-one.
        let pk: HashSet<&str> = t
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.as_str())
            .collect();
        for fk in &t.foreign_keys {
            let pairs: Vec<Vec<u8>> = fk
                .columns
                .iter()
                .zip(&fk.foreign_columns)
                .map(|(col, fcol)| {
                    composite(&[
                        (OID_TEXT, Some(col.clone().into_bytes())),
                        (OID_TEXT, Some(fcol.clone().into_bytes())),
                    ])
                })
                .collect();
            let one_to_one = !pk.is_empty()
                && fk
                    .columns
                    .iter()
                    .map(String::as_str)
                    .collect::<HashSet<_>>()
                    == pk;
            rows.push(vec![
                Value::Text(SCHEMA.to_string()),
                Value::Text(t.name.clone()),
                Value::Text(SCHEMA.to_string()),
                Value::Text(fk.foreign_table.clone()),
                Value::Blob(bin_bool(fk.foreign_table.eq_ignore_ascii_case(&t.name))),
                Value::Text(fk.name.clone()),
                Value::Blob(record_array(&pairs)),
                Value::Blob(bin_bool(one_to_one)),
            ]);
        }
    }

    Canned::Rows {
        tag: format!("SELECT {}", rows.len()),
        columns,
        oids,
        rows,
    }
}

/// Encode one column as the 8-field composite PostgREST decodes.
fn column_composite(c: &engine::CatalogColumn) -> Vec<u8> {
    let fields: Vec<(i32, Option<Vec<u8>>)> = vec![
        (OID_TEXT, Some(c.name.clone().into_bytes())), // column_name
        (OID_TEXT, None),                              // description
        (OID_BOOL, Some(bin_bool(!c.not_null))),       // is_nullable
        (OID_TEXT, Some(c.pg_type.as_bytes().to_vec())), // data_type
        (OID_TEXT, Some(c.pg_type.as_bytes().to_vec())), // nominal_data_type
        (OID_INT4, None),                              // character_maximum_length
        (OID_TEXT, None),                              // column_default
        (OID_TEXT_ARR, Some(text_array(&[]))),         // enum_labels (none)
    ];
    composite(&fields)
}

// ---- Postgres binary wire encoders ----------------------------------------

fn bin_bool(b: bool) -> Vec<u8> {
    vec![b as u8]
}

/// A binary composite/record: `int32 nfields`, then per field
/// `int32 oid, int32 len (-1=null), bytes`.
fn composite(fields: &[(i32, Option<Vec<u8>>)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(fields.len() as i32).to_be_bytes());
    for (oid, val) in fields {
        b.extend_from_slice(&oid.to_be_bytes());
        match val {
            None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                b.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                b.extend_from_slice(bytes);
            }
        }
    }
    b
}

/// A 1-dimensional binary array of text. Empty → a 0-dimension array.
fn text_array(items: &[&str]) -> Vec<u8> {
    let elems: Vec<Vec<u8>> = items.iter().map(|s| s.as_bytes().to_vec()).collect();
    array(OID_TEXT, &elems)
}

/// A 1-dimensional binary array of (already-encoded) record elements.
fn record_array(records: &[Vec<u8>]) -> Vec<u8> {
    array(OID_RECORD, records)
}

/// A 1-dimensional, non-null binary array with the given element OID. Each
/// element is the already-encoded value bytes. An empty slice yields the
/// 0-dimension form Postgres uses for `'{}'`.
fn array(elem_oid: i32, elems: &[Vec<u8>]) -> Vec<u8> {
    let mut b = Vec::new();
    let ndim: i32 = if elems.is_empty() { 0 } else { 1 };
    b.extend_from_slice(&ndim.to_be_bytes());
    b.extend_from_slice(&0i32.to_be_bytes()); // flags (no nulls)
    b.extend_from_slice(&elem_oid.to_be_bytes());
    if !elems.is_empty() {
        b.extend_from_slice(&(elems.len() as i32).to_be_bytes()); // dim length
        b.extend_from_slice(&1i32.to_be_bytes()); // lower bound
        for e in elems {
            b.extend_from_slice(&(e.len() as i32).to_be_bytes());
            b.extend_from_slice(e);
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::{CatalogColumn, CatalogForeignKey};

    #[test]
    fn empty_array_is_zero_dim() {
        // ndim=0, flags=0, elem_oid — 12 bytes, no dimension/element data.
        let a = text_array(&[]);
        assert_eq!(a.len(), 12);
        assert_eq!(a[0..4], 0i32.to_be_bytes());
        assert_eq!(a[8..12], OID_TEXT.to_be_bytes());
    }

    #[test]
    fn tables_reflection_shape() {
        let catalog = vec![CatalogTable {
            name: "books".into(),
            columns: vec![
                CatalogColumn {
                    name: "id".into(),
                    pg_type: "bigint",
                    ty: engine::ColumnType::Integer,
                    not_null: true,
                    primary_key: true,
                    position: 1,
                },
                CatalogColumn {
                    name: "title".into(),
                    pg_type: "text",
                    ty: engine::ColumnType::Text,
                    not_null: false,
                    primary_key: false,
                    position: 2,
                },
            ],
            foreign_keys: vec![],
        }];
        match reflect(ReflectKind::Tables, &catalog) {
            Canned::Rows {
                columns,
                oids,
                rows,
                ..
            } => {
                assert_eq!(columns.len(), 9);
                assert_eq!(oids.len(), 9);
                assert_eq!(oids[7], OID_TEXT_ARR); // pk_cols
                assert_eq!(oids[8], OID_RECORD_ARR); // columns
                assert_eq!(rows.len(), 1);
                let row = &rows[0];
                assert!(matches!(&row[1], Value::Text(t) if t == "books"));
                // pk_cols + columns are non-empty binary arrays.
                assert!(matches!(&row[7], Value::Blob(b) if !b.is_empty()));
                assert!(matches!(&row[8], Value::Blob(b) if b.len() > 12));
            }
            _ => panic!("Tables reflection must produce Rows"),
        }
    }

    #[test]
    fn relationships_reflection_shape() {
        // books.author_id -> authors.id : a many-to-one relationship.
        let catalog = vec![
            CatalogTable {
                name: "authors".into(),
                columns: vec![CatalogColumn {
                    name: "id".into(),
                    pg_type: "bigint",
                    ty: engine::ColumnType::Integer,
                    not_null: true,
                    primary_key: true,
                    position: 1,
                }],
                foreign_keys: vec![],
            },
            CatalogTable {
                name: "books".into(),
                columns: vec![
                    CatalogColumn {
                        name: "id".into(),
                        pg_type: "bigint",
                        ty: engine::ColumnType::Integer,
                        not_null: true,
                        primary_key: true,
                        position: 1,
                    },
                    CatalogColumn {
                        name: "author_id".into(),
                        pg_type: "bigint",
                        ty: engine::ColumnType::Integer,
                        not_null: false,
                        primary_key: false,
                        position: 2,
                    },
                ],
                foreign_keys: vec![CatalogForeignKey {
                    name: "books_author_id_fkey".into(),
                    columns: vec!["author_id".into()],
                    foreign_table: "authors".into(),
                    foreign_columns: vec!["id".into()],
                }],
            },
        ];
        match reflect(ReflectKind::Relationships, &catalog) {
            Canned::Rows {
                columns,
                oids,
                rows,
                tag,
            } => {
                assert_eq!(columns.len(), 8);
                assert_eq!(oids[6], OID_RECORD_ARR); // cols_and_fcols
                assert_eq!(tag, "SELECT 1");
                assert_eq!(rows.len(), 1);
                let row = &rows[0];
                assert!(matches!(&row[1], Value::Text(t) if t == "books"));
                assert!(matches!(&row[3], Value::Text(t) if t == "authors"));
                assert!(matches!(&row[4], Value::Blob(b) if b == &[0])); // is_self = false
                assert!(matches!(&row[5], Value::Text(t) if t == "books_author_id_fkey"));
                assert!(matches!(&row[6], Value::Blob(b) if b.len() > 12)); // non-empty record[]
                assert!(matches!(&row[7], Value::Blob(b) if b == &[0])); // one_to_one = false
            }
            _ => panic!("Relationships reflection must produce Rows"),
        }
    }
}
