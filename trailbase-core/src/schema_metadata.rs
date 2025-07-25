use fallible_iterator::FallibleIterator;
use log::*;
use std::collections::HashSet;
use std::sync::Arc;
use thiserror::Error;
use trailbase_schema::parse::parse_into_statement;
use trailbase_schema::sqlite::{QualifiedName, SchemaError, Table, View};
use trailbase_sqlite::params;

pub use trailbase_schema::metadata::{
  JsonColumnMetadata, JsonSchemaError, TableMetadata, TableOrViewMetadata, ViewMetadata,
};

use crate::constants::{SQLITE_SCHEMA_TABLE, USER_TABLE};

struct SchemaMetadataCacheState {
  tables: HashSet<Arc<TableMetadata>>,
  views: HashSet<Arc<ViewMetadata>>,
}

#[derive(Clone)]
pub struct SchemaMetadataCache {
  conn: trailbase_sqlite::Connection,
  state: Arc<parking_lot::RwLock<SchemaMetadataCacheState>>,
}

impl SchemaMetadataCache {
  pub async fn new(conn: trailbase_sqlite::Connection) -> Result<Self, SchemaLookupError> {
    let tables = lookup_and_parse_all_table_schemas(&conn).await?;
    let table_map = Self::build_tables(&conn, &tables).await?;
    let views = Self::build_views(&conn, &tables).await?;

    return Ok(SchemaMetadataCache {
      conn,
      state: Arc::new(parking_lot::RwLock::new(SchemaMetadataCacheState {
        tables: table_map,
        views,
      })),
    });
  }

  async fn build_tables(
    conn: &trailbase_sqlite::Connection,
    tables: &[Table],
  ) -> Result<HashSet<Arc<TableMetadata>>, SchemaLookupError> {
    let schema_metadata_map: HashSet<Arc<TableMetadata>> = tables
      .iter()
      .cloned()
      .map(|t: Table| {
        return Arc::new(TableMetadata::new(t, tables, USER_TABLE));
      })
      .collect();

    // Install file column triggers. This ain't pretty, this might be better on construction and
    // schema changes.
    for metadata in &schema_metadata_map {
      for idx in metadata.json_metadata.file_column_indexes() {
        let table_name = &metadata.schema.name;
        let unqualified_name = &metadata.schema.name.name;
        let db = metadata
          .schema
          .name
          .database_schema
          .as_deref()
          .unwrap_or("main");

        if db != "main" {
          // FIXME: TRIGGERS are always database-local. Thus every database with tables
          // with file columns would need its own _file_deletions table.
          return Err(SchemaLookupError::Other(
            "File columns not suppored on attached databases".into(),
          ));
        }

        let col = &metadata.schema.columns[*idx];
        let column_name = &col.name;

        conn.execute_batch(indoc::formatdoc!(
          r#"
          DROP TRIGGER IF EXISTS "{db}"."__{unqualified_name}__{column_name}__update_trigger";
          CREATE TRIGGER IF NOT EXISTS "{db}"."__{unqualified_name}__{column_name}__update_trigger" AFTER UPDATE ON {table_name}
            WHEN OLD."{column_name}" IS NOT NULL AND OLD."{column_name}" != NEW."{column_name}"
            BEGIN
              INSERT INTO _file_deletions (table_name, record_rowid, column_name, json) VALUES
                ('{table_name}', OLD._rowid_, '{column_name}', OLD."{column_name}");
            END;

          DROP TRIGGER IF EXISTS "{db}"."__{unqualified_name}__{column_name}__delete_trigger";
          CREATE TRIGGER IF NOT EXISTS "{db}"."__{unqualified_name}__{column_name}__delete_trigger" AFTER DELETE ON {table_name}
            WHEN OLD."{column_name}" IS NOT NULL
            BEGIN
              INSERT INTO _file_deletions (table_name, record_rowid, column_name, json) VALUES
                ('{table_name}', OLD._rowid_, '{column_name}', OLD."{column_name}");
            END;
          "#,
          table_name = table_name.escaped_string(),
        )).await?;
      }
    }

    return Ok(schema_metadata_map);
  }

  async fn build_views(
    conn: &trailbase_sqlite::Connection,
    tables: &[Table],
  ) -> Result<HashSet<Arc<ViewMetadata>>, SchemaLookupError> {
    let views = lookup_and_parse_all_view_schemas(conn, tables).await?;
    let build = |view: View| {
      // NOTE: we check during record API config validation that no temporary views are referenced.
      // if view.temporary {
      //   debug!("Temporary view: {}", view.name);
      // }

      return Some(Arc::new(ViewMetadata::new(view, tables)));
    };

    return Ok(views.into_iter().filter_map(build).collect());
  }

  pub fn get_table(&self, name: &QualifiedName) -> Option<Arc<TableMetadata>> {
    return self.state.read().tables.get(name).cloned();
  }

  pub fn get_view(&self, name: &QualifiedName) -> Option<Arc<ViewMetadata>> {
    self.state.read().views.get(name).cloned()
  }

  pub(crate) fn tables(&self) -> Vec<TableMetadata> {
    return self
      .state
      .read()
      .tables
      .iter()
      .map(|t| (**t).clone())
      .collect();
  }

  pub async fn invalidate_all(&self) -> Result<(), SchemaLookupError> {
    debug!("Rebuilding SchemaMetadataCache");
    let conn = &self.conn;

    let tables = lookup_and_parse_all_table_schemas(conn).await?;
    let table_map = Self::build_tables(conn, &tables).await?;
    let views = Self::build_views(conn, &tables).await?;

    *self.state.write() = SchemaMetadataCacheState {
      tables: table_map,
      views,
    };

    Ok(())
  }
}

impl std::fmt::Debug for SchemaMetadataCache {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let state = self.state.read();
    f.debug_struct("SchemaMetadataCache")
      .field("tables", &state.tables.iter().map(|t| &t.schema.name))
      .field("views", &state.views.iter().map(|v| &v.schema.name))
      .finish()
  }
}

#[derive(Debug, Error)]
pub enum SchemaLookupError {
  #[error("TB SQLite error: {0}")]
  Sql(#[from] trailbase_sqlite::Error),
  #[error("Rusqlite error: {0}")]
  FromSql(#[from] rusqlite::types::FromSqlError),
  #[error("Schema error: {0}")]
  Schema(#[from] SchemaError),
  #[error("Missing")]
  Missing,
  #[error("Sql parse error: {0}")]
  SqlParse(#[from] sqlite3_parser::lexer::sql::Error),
  #[error("Other error: {0}")]
  Other(Box<dyn std::error::Error + Send + Sync>),
}

pub async fn lookup_and_parse_table_schema(
  conn: &trailbase_sqlite::Connection,
  table_name: &str,
  database: Option<&str>,
) -> Result<Table, SchemaLookupError> {
  // Then get the actual table.
  let sql: String = conn
    .read_query_row_f(
      format!(
        "SELECT sql FROM {db}.{SQLITE_SCHEMA_TABLE} WHERE type = 'table' AND name = $1",
        db = database.unwrap_or("main")
      ),
      params!(table_name.to_string()),
      |row| row.get(0),
    )
    .await?
    .ok_or_else(|| trailbase_sqlite::Error::Rusqlite(rusqlite::Error::QueryReturnedNoRows))?;

  let Some(stmt) = parse_into_statement(&sql)? else {
    return Err(SchemaLookupError::Missing);
  };

  let mut table: Table = stmt.try_into()?;
  if let Some(database) = database {
    table.name.database_schema = Some(database.to_string());
  }
  return Ok(table);
}

pub async fn lookup_and_parse_all_table_schemas(
  conn: &trailbase_sqlite::Connection,
) -> Result<Vec<Table>, SchemaLookupError> {
  let databases = conn.list_databases().await?;

  let mut tables: Vec<Table> = vec![];
  for db in databases {
    // Then get the actual tables.
    let rows = conn
      .read_query_rows(
        format!(
          "SELECT sql FROM {db}.{SQLITE_SCHEMA_TABLE} WHERE type = 'table'",
          db = db.name
        ),
        (),
      )
      .await?;

    for row in rows.iter() {
      let sql: String = row.get(0)?;
      let Some(stmt) = parse_into_statement(&sql)? else {
        return Err(SchemaLookupError::Missing);
      };
      tables.push({
        let mut table: Table = stmt.try_into()?;
        table.name.database_schema = Some(db.name.clone());
        table
      });
    }
  }

  return Ok(tables);
}

fn sqlite3_parse_view(sql: &str, tables: &[Table]) -> Result<View, SchemaLookupError> {
  let mut parser = sqlite3_parser::lexer::sql::Parser::new(sql.as_bytes());
  match parser.next()? {
    None => Err(SchemaLookupError::Missing),
    Some(cmd) => {
      use sqlite3_parser::ast::Cmd;
      match cmd {
        Cmd::Stmt(stmt) => Ok(View::from(stmt, tables)?),
        Cmd::Explain(_) | Cmd::ExplainQueryPlan(_) => Err(SchemaLookupError::Missing),
      }
    }
  }
}

pub async fn lookup_and_parse_all_view_schemas(
  conn: &trailbase_sqlite::Connection,
  tables: &[Table],
) -> Result<Vec<View>, SchemaLookupError> {
  let databases = conn.list_databases().await?;

  let mut views: Vec<View> = vec![];
  for db in databases {
    // Then get the actual views.
    let rows = conn
      .read_query_rows(
        format!("SELECT sql FROM {SQLITE_SCHEMA_TABLE} WHERE type = 'view'"),
        (),
      )
      .await?;

    for row in rows.iter() {
      let sql: String = row.get(0)?;
      match sqlite3_parse_view(&sql, tables) {
        Ok(mut view) => {
          view.name.database_schema = Some(db.name.clone());
          views.push(view);
        }
        Err(err) => {
          error!("Failed to parse VIEW definition '{sql}': {err}");
        }
      }
    }
  }

  return Ok(views);
}

#[cfg(test)]
mod tests {
  use axum::extract::{Json, Path, Query, RawQuery, State};
  use serde_json::json;
  use trailbase_schema::QualifiedName;
  use trailbase_schema::json_schema::{Expand, JsonSchemaMode, build_json_schema_expanded};

  use crate::app_state::*;
  use crate::config::proto::{PermissionFlag, RecordApiConfig};
  use crate::records::list_records::list_records_handler;
  use crate::records::read_record::{ReadRecordQuery, read_record_handler};
  use crate::records::test_utils::add_record_api_config;

  #[tokio::test]
  async fn test_expanded_foreign_key() {
    let state = test_state(None).await.unwrap();
    let conn = state.conn();

    conn
      .execute(
        "CREATE TABLE foreign_table (id INTEGER PRIMARY KEY) STRICT",
        (),
      )
      .await
      .unwrap();

    let table_name = QualifiedName {
      name: "test_table".to_string(),
      database_schema: None,
    };

    conn
      .execute(
        format!(
          r#"CREATE TABLE {table_name} (
            id INTEGER PRIMARY KEY,
            fk INTEGER REFERENCES foreign_table(id)
          ) STRICT"#,
          table_name = table_name.escaped_string(),
        ),
        (),
      )
      .await
      .unwrap();

    state.schema_metadata().invalidate_all().await.unwrap();

    add_record_api_config(
      &state,
      RecordApiConfig {
        name: Some("test_table_api".to_string()),
        table_name: Some(table_name.name.clone()),
        acl_world: [PermissionFlag::Create as i32, PermissionFlag::Read as i32].into(),
        expand: vec!["fk".to_string()],
        ..Default::default()
      },
    )
    .await
    .unwrap();

    let test_schema_metadata = state.schema_metadata().get_table(&table_name).unwrap();

    let (validator, schema) = build_json_schema_expanded(
      &table_name.name,
      &test_schema_metadata.schema.columns,
      JsonSchemaMode::Select,
      Some(Expand {
        tables: &state.schema_metadata().tables(),
        foreign_key_columns: vec!["foreign_table"],
      }),
    )
    .unwrap();

    assert_eq!(
      schema,
      json!({
        "title": table_name.name,
        "type": "object",
        "properties": {
          "id": { "type": "integer" },
          "fk": { "$ref": "#/$defs/foreign_table" },
        },
        "required": ["id"],
        "$defs": {
          "foreign_table": {
            "type": "object",
            "properties": {
              "id" : { "type": "integer"},
              "data": {
                "title": "foreign_table",
                "type": "object",
                "properties": {
                  "id" : { "type": "integer" },
                },
                "required": ["id"],
              },
            },
            "required": ["id"],
          },
        },
      })
    );

    conn
      .execute("INSERT INTO foreign_table (id) VALUES (1);", ())
      .await
      .unwrap();

    conn
      .execute(
        format!(
          "INSERT INTO {table_name} (id, fk) VALUES (1, 1);",
          table_name = table_name.escaped_string(),
        ),
        (),
      )
      .await
      .unwrap();

    // Expansion of invalid column.
    {
      let response = read_record_handler(
        State(state.clone()),
        Path(("test_table_api".to_string(), "1".to_string())),
        Query(ReadRecordQuery {
          expand: Some("UNKNOWN".to_string()),
        }),
        None,
      )
      .await;

      assert!(response.is_err());

      let list_response = list_records_handler(
        State(state.clone()),
        Path("test_table_api".to_string()),
        RawQuery(Some("expand=UNKNOWN".to_string())),
        None,
      )
      .await;

      assert!(list_response.is_err());
    }

    // Not expanded
    {
      let expected = json!({
        "id": 1,
        "fk":{ "id": 1 },
      });

      let Json(value) = read_record_handler(
        State(state.clone()),
        Path(("test_table_api".to_string(), "1".to_string())),
        Query(ReadRecordQuery::default()),
        None,
      )
      .await
      .unwrap();

      validator.validate(&value).expect(&format!("{value}"));

      assert_eq!(expected, value);

      let Json(list_response) = list_records_handler(
        State(state.clone()),
        Path("test_table_api".to_string()),
        RawQuery(None),
        None,
      )
      .await
      .unwrap();

      assert_eq!(vec![expected.clone()], list_response.records);
      validator.validate(&list_response.records[0]).unwrap();
    }

    let expected = json!({
      "id": 1,
      "fk":{
        "id": 1,
        "data": {
          "id": 1,
        },
      },
    });

    {
      let Json(value) = read_record_handler(
        State(state.clone()),
        Path(("test_table_api".to_string(), "1".to_string())),
        Query(ReadRecordQuery {
          expand: Some("fk".to_string()),
        }),
        None,
      )
      .await
      .unwrap();

      validator.validate(&value).expect(&format!("{value}"));

      assert_eq!(expected, value);
    }

    {
      let Json(list_response) = list_records_handler(
        State(state.clone()),
        Path("test_table_api".to_string()),
        RawQuery(Some("expand=fk".to_string())),
        None,
      )
      .await
      .unwrap();

      assert_eq!(vec![expected.clone()], list_response.records);
      validator.validate(&list_response.records[0]).unwrap();
    }

    {
      let Json(list_response) = list_records_handler(
        State(state.clone()),
        Path("test_table_api".to_string()),
        RawQuery(Some("count=TRUE&expand=fk".to_string())),
        None,
      )
      .await
      .unwrap();

      assert_eq!(Some(1), list_response.total_count);
      assert_eq!(vec![expected], list_response.records);
      validator.validate(&list_response.records[0]).unwrap();
    }
  }

  #[tokio::test]
  async fn test_expanded_with_multiple_foreign_keys() {
    let state = test_state(None).await.unwrap();

    let exec = {
      let conn = state.conn();
      move |sql: &str| {
        let conn = conn.clone();
        let owned = sql.to_string();
        return async move { conn.execute(owned, ()).await };
      }
    };

    exec("CREATE TABLE foreign_table0 (id INTEGER PRIMARY KEY) STRICT")
      .await
      .unwrap();
    exec("CREATE TABLE foreign_table1 (id INTEGER PRIMARY KEY) STRICT")
      .await
      .unwrap();

    let table_name = "test_table";
    exec(&format!(
      r#"CREATE TABLE {table_name} (
          id        INTEGER PRIMARY KEY,
          fk0       INTEGER REFERENCES foreign_table0(id),
          fk0_null  INTEGER REFERENCES foreign_table0(id),
          fk1       INTEGER REFERENCES foreign_table1(id)
        ) STRICT"#
    ))
    .await
    .unwrap();

    state.schema_metadata().invalidate_all().await.unwrap();

    add_record_api_config(
      &state,
      RecordApiConfig {
        name: Some("test_table_api".to_string()),
        table_name: Some(table_name.to_string()),
        acl_world: [PermissionFlag::Create as i32, PermissionFlag::Read as i32].into(),
        expand: vec!["fk0".to_string(), "fk1".to_string()],
        ..Default::default()
      },
    )
    .await
    .unwrap();

    exec("INSERT INTO foreign_table0 (id) VALUES (1);")
      .await
      .unwrap();
    exec("INSERT INTO foreign_table1 (id) VALUES (1);")
      .await
      .unwrap();

    exec(&format!(
      "INSERT INTO {table_name} (id, fk0, fk0_null, fk1) VALUES (1, 1, NULL, 1);"
    ))
    .await
    .unwrap();

    // Expand none
    {
      let Json(value) = read_record_handler(
        State(state.clone()),
        Path(("test_table_api".to_string(), "1".to_string())),
        Query(ReadRecordQuery { expand: None }),
        None,
      )
      .await
      .unwrap();

      let expected = json!({
        "id": 1,
        "fk0": { "id": 1 },
        "fk0_null": serde_json::Value::Null,
        "fk1": { "id": 1 },
      });

      assert_eq!(expected, value);

      let Json(list_response) = list_records_handler(
        State(state.clone()),
        Path("test_table_api".to_string()),
        RawQuery(None),
        None,
      )
      .await
      .unwrap();

      assert_eq!(vec![expected], list_response.records);
    }

    // Expand one
    {
      let expected = json!({
        "id": 1,
        "fk0": { "id": 1 },
        "fk0_null": serde_json::Value::Null,
        "fk1": {
          "id": 1,
          "data": {
            "id": 1,
          },
        },
      });

      let Json(value) = read_record_handler(
        State(state.clone()),
        Path(("test_table_api".to_string(), "1".to_string())),
        Query(ReadRecordQuery {
          expand: Some("fk1".to_string()),
        }),
        None,
      )
      .await
      .unwrap();

      assert_eq!(expected, value);

      let Json(list_response) = list_records_handler(
        State(state.clone()),
        Path("test_table_api".to_string()),
        RawQuery(Some("expand=fk1".to_string())),
        None,
      )
      .await
      .unwrap();

      assert_eq!(vec![expected], list_response.records);
    }

    // Expand all.
    {
      let expected = json!({
        "id": 1,
        "fk0": {
          "id": 1,
          "data": {
            "id": 1,
          },
        },
        "fk0_null": serde_json::Value::Null,
        "fk1": {
          "id": 1,
          "data": {
            "id": 1,
          },
        },
      });

      let Json(value) = read_record_handler(
        State(state.clone()),
        Path(("test_table_api".to_string(), "1".to_string())),
        Query(ReadRecordQuery {
          expand: Some("fk0,fk1".to_string()),
        }),
        None,
      )
      .await
      .unwrap();

      assert_eq!(expected, value);

      exec(&format!("INSERT INTO {table_name} (id) VALUES (2);"))
        .await
        .unwrap();

      let Json(list_response) = list_records_handler(
        State(state.clone()),
        Path("test_table_api".to_string()),
        RawQuery(Some("expand=fk0,fk1".to_string())),
        None,
      )
      .await
      .unwrap();

      assert_eq!(
        vec![
          json!({
            "id": 2,
            "fk0": serde_json::Value::Null,
            "fk0_null":  serde_json::Value::Null,
            "fk1":  serde_json::Value::Null,
          }),
          expected
        ],
        list_response.records
      );
    }
  }
}
