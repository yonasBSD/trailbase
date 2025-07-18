use std::collections::HashSet;

use axum::{
  Json,
  extract::State,
  http::StatusCode,
  response::{IntoResponse, Response},
};
use log::*;
use serde::Deserialize;
use trailbase_schema::sqlite::{QualifiedName, Table};
use ts_rs::TS;

use crate::admin::AdminError as Error;
use crate::app_state::AppState;
use crate::config::proto::hash_config;
use crate::transaction::{TransactionLog, TransactionRecorder};

#[derive(Clone, Debug, Deserialize, TS)]
#[ts(export)]
pub struct AlterTableRequest {
  pub source_schema: Table,
  pub target_schema: Table,
}

// NOTE: sqlite has very limited alter table support, thus we're always recreating the table and
// moving data over, see https://sqlite.org/lang_altertable.html.

pub async fn alter_table_handler(
  State(state): State<AppState>,
  Json(request): Json<AlterTableRequest>,
) -> Result<Response, Error> {
  if state.demo_mode() && request.source_schema.name.name.starts_with("_") {
    return Err(Error::Precondition("Disallowed in demo".into()));
  }
  if request.source_schema.name.database_schema != request.target_schema.name.database_schema {
    return Err(Error::Precondition("Cannot move between databases".into()));
  }

  let source_schema = request.source_schema;
  let source_table_name = source_schema.name.clone();
  let filename = source_table_name.migration_filename("alter_table");

  let Some(_metadata) = state.schema_metadata().get_table(&source_table_name) else {
    return Err(Error::Precondition(format!(
      "Cannot alter '{source_table_name:?}'. Only tables are supported.",
    )));
  };

  let target_schema = request.target_schema;
  let target_table_name = target_schema.name.clone();

  debug!("Alter table:\nsource: {source_schema:?}\ntarget: {target_schema:?}",);

  let is_table_rename = target_table_name != source_table_name;
  let temp_table_name: QualifiedName = if is_table_rename {
    // No ephemeral table needed.
    target_table_name.clone()
  } else {
    QualifiedName {
      name: format!("__alter_table_{}", target_table_name.name),
      database_schema: target_table_name.database_schema.clone(),
    }
  };

  let source_columns: HashSet<String> = source_schema
    .columns
    .iter()
    .map(|c| c.name.clone())
    .collect();
  let copy_columns: Vec<String> = target_schema
    .columns
    .iter()
    .filter_map(|c| {
      if source_columns.contains(&c.name) {
        Some(c.name.clone())
      } else {
        None
      }
    })
    .collect();

  let mut target_schema_copy = target_schema.clone();
  target_schema_copy.name = temp_table_name.clone();

  let conn = state.conn();
  let log = conn
    .call(
      move |conn| -> Result<Option<TransactionLog>, trailbase_sqlite::Error> {
        let mut tx = TransactionRecorder::new(conn)
          .map_err(|err| trailbase_sqlite::Error::Other(err.into()))?;

        tx.execute("PRAGMA foreign_keys = OFF", ())?;

        // Create new table
        let sql = target_schema_copy.create_table_statement();
        tx.execute(&sql, ())?;

        // Copy
        tx.execute(
          &format!(
            r#"
            INSERT INTO
              {temp_table_name} ({column_list})
            SELECT
              {column_list}
            FROM
              {source_table_name}
          "#,
            column_list = copy_columns.join(", "),
            temp_table_name = temp_table_name.escaped_string(),
            source_table_name = source_table_name.escaped_string(),
          ),
          (),
        )?;

        tx.execute(
          &format!(
            "DROP TABLE {source_table_name}",
            source_table_name = source_table_name.escaped_string()
          ),
          (),
        )?;

        if target_table_name != temp_table_name {
          tx.execute(
            &format!(
              "ALTER TABLE {temp_table_name} RENAME TO {target_table_name}",
              temp_table_name = temp_table_name.escaped_string(),
              target_table_name = target_table_name.escaped_string()
            ),
            (),
          )?;
        }

        tx.execute("PRAGMA foreign_keys = ON", ())?;

        return tx
          .rollback()
          .map_err(|err| trailbase_sqlite::Error::Other(err.into()));
      },
    )
    .await?;

  // Write migration file and apply it right away.
  if let Some(log) = log {
    let migration_path = state.data_dir().migrations_path();
    let report = log
      .apply_as_migration(state.conn(), migration_path, &filename)
      .await?;
    debug!("Migration report: {report:?}");
  }

  state.schema_metadata().invalidate_all().await?;

  // Fix configuration: update all table references by existing APIs.
  //
  // TODO: Update and or validate for column changes. Specifically: expand, excluded_columns and
  // all the rules.
  if is_table_rename
    && matches!(
      source_schema.name.database_schema.as_deref(),
      Some("main") | None
    )
  {
    let mut config = state.get_config();
    let old_config_hash = hash_config(&config);

    for api in &mut config.record_apis {
      if let Some(ref name) = api.table_name {
        if *name == source_schema.name.name {
          api.table_name = Some(target_schema.name.name.clone());
        }
      }
    }

    state
      .validate_and_update_config(config, Some(old_config_hash))
      .await?;
  }

  return Ok((StatusCode::OK, "altered table").into_response());
}

#[cfg(test)]
mod tests {
  use trailbase_schema::sqlite::{Column, ColumnDataType, ColumnOption, Table};

  use super::*;
  use crate::admin::table::{CreateTableRequest, create_table_handler};
  use crate::app_state::*;

  #[tokio::test]
  async fn test_alter_table() -> Result<(), anyhow::Error> {
    let state = test_state(None).await?;
    let conn = state.conn();
    let pk_col = "my_pk".to_string();

    let create_table_request = CreateTableRequest {
      schema: Table {
        name: QualifiedName::parse("foo").unwrap(),
        strict: true,
        columns: vec![Column {
          name: pk_col.clone(),
          data_type: ColumnDataType::Blob,
          options: vec![ColumnOption::Unique {
            is_primary: true,
            conflict_clause: None,
          }],
        }],
        foreign_keys: vec![],
        unique: vec![],
        checks: vec![],
        virtual_table: false,
        temporary: false,
      },
      dry_run: Some(false),
    };
    debug!(
      "Create Table: {}",
      create_table_request.schema.create_table_statement()
    );
    let _ = create_table_handler(State(state.clone()), Json(create_table_request.clone())).await?;

    conn
      .read_query_rows(format!("SELECT {pk_col} FROM foo"), ())
      .await?;

    {
      // Noop: source and target identical.
      let alter_table_request = AlterTableRequest {
        source_schema: create_table_request.schema.clone(),
        target_schema: create_table_request.schema.clone(),
      };

      alter_table_handler(State(state.clone()), Json(alter_table_request.clone()))
        .await
        .unwrap();

      conn
        .read_query_rows(format!("SELECT {pk_col} FROM foo"), ())
        .await?;
    }

    {
      // Add column.
      let mut target_schema = create_table_request.schema.clone();

      target_schema.columns.push(Column {
        name: "new".to_string(),
        data_type: ColumnDataType::Text,
        options: vec![
          ColumnOption::NotNull,
          ColumnOption::Default("'default'".to_string()),
        ],
      });

      debug!("{}", target_schema.create_table_statement());

      let alter_table_request = AlterTableRequest {
        source_schema: create_table_request.schema.clone(),
        target_schema,
      };

      alter_table_handler(State(state.clone()), Json(alter_table_request.clone()))
        .await
        .unwrap();

      conn
        .read_query_rows(format!("SELECT {pk_col}, new FROM foo"), ())
        .await?;
    }

    {
      // Rename table and remove "new" column.
      let mut target_schema = create_table_request.schema.clone();

      target_schema.name = QualifiedName::parse("bar").unwrap();

      debug!("{}", target_schema.create_table_statement());

      let alter_table_request = AlterTableRequest {
        source_schema: create_table_request.schema.clone(),
        target_schema,
      };

      alter_table_handler(State(state.clone()), Json(alter_table_request.clone()))
        .await
        .unwrap();

      assert!(conn.read_query_rows("SELECT * FROM foo", ()).await.is_err());
      conn
        .read_query_rows(format!("SELECT {pk_col} FROM bar"), ())
        .await?;
    }

    return Ok(());
  }
}
