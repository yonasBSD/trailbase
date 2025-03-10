use base64::prelude::*;
use itertools::Itertools;
use log::*;
use object_store::ObjectStore;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use trailbase_sqlite::schema::{FileUpload, FileUploadInput, FileUploads};
use trailbase_sqlite::{NamedParams, Params as _, Value};

use crate::config::proto::ConflictResolutionStrategy;
use crate::records::error::RecordError;
use crate::records::files::delete_files_in_row;
use crate::schema::{Column, ColumnDataType, ColumnOption};
use crate::table_metadata::{
  self, ColumnMetadata, JsonColumnMetadata, TableMetadata, TableMetadataCache,
};
use crate::AppState;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ParamsError {
  #[error("Not an object")]
  NotAnObject,
  #[error("Not a Number")]
  NotANumber,
  #[error("Column error: {0}")]
  Column(&'static str),
  #[error("Unexpected type: {0}, expected {1}")]
  UnexpectedType(&'static str, String),
  #[error("Decoding error: {0}")]
  Decode(#[from] base64::DecodeError),
  #[error("Nested object: {0}")]
  NestedObject(String),
  #[error("Nested array: {0}")]
  NestedArray(String),
  #[error("Inhomogenous array: {0}")]
  InhomogenousArray(String),
  #[error("Parse int error: {0}")]
  ParseInt(#[from] std::num::ParseIntError),
  #[error("Parse float error: {0}")]
  ParseFloat(#[from] std::num::ParseFloatError),
  #[error("Json validation error: {0}")]
  JsonValidation(#[from] table_metadata::JsonSchemaError),
  #[error("Json serialization error: {0}")]
  JsonSerialization(Arc<serde_json::Error>),
  #[error("Json schema error: {0}")]
  Schema(#[from] trailbase_sqlite::schema::SchemaError),
  #[error("ObjectStore error: {0}")]
  Storage(Arc<object_store::Error>),
}

impl From<serde_json::Error> for ParamsError {
  fn from(err: serde_json::Error) -> Self {
    return Self::JsonSerialization(Arc::new(err));
  }
}

impl From<object_store::Error> for ParamsError {
  fn from(err: object_store::Error) -> Self {
    return Self::Storage(Arc::new(err));
  }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum QueryError {
  #[error("Precondition error: {0}")]
  Precondition(&'static str),
  #[error("Sql error: {0}")]
  Sql(Arc<rusqlite::Error>),
  #[error("FromSql error: {0}")]
  FromSql(Arc<rusqlite::types::FromSqlError>),
  #[error("Tokio Rusqlite error: {0}")]
  TokioRusqlite(Arc<trailbase_sqlite::Error>),
  #[error("Json serialization error: {0}")]
  JsonSerialization(Arc<serde_json::Error>),
  #[error("ObjectStore error: {0}")]
  Storage(Arc<object_store::Error>),
  #[error("File error: {0}")]
  File(Arc<crate::records::files::FileError>),
  #[error("Not found")]
  NotFound,
}

impl From<serde_json::Error> for QueryError {
  fn from(err: serde_json::Error) -> Self {
    return Self::JsonSerialization(err.into());
  }
}

impl From<trailbase_sqlite::Error> for QueryError {
  fn from(err: trailbase_sqlite::Error) -> Self {
    return Self::TokioRusqlite(err.into());
  }
}

impl From<rusqlite::types::FromSqlError> for QueryError {
  fn from(err: rusqlite::types::FromSqlError) -> Self {
    return Self::FromSql(err.into());
  }
}

impl From<object_store::Error> for QueryError {
  fn from(err: object_store::Error) -> Self {
    return Self::Storage(err.into());
  }
}

impl From<crate::records::files::FileError> for QueryError {
  fn from(err: crate::records::files::FileError) -> Self {
    return Self::File(err.into());
  }
}

type FileMetadataContents = Vec<(FileUpload, Vec<u8>)>;

// JSON type use to represent rows. Note that we use a map to represent rows sparsely.
pub type JsonRow = serde_json::Map<String, serde_json::Value>;

#[derive(Default)]
pub struct Params {
  table_name: String,

  /// List of named params with their respective placeholders, e.g.:
  ///   '(":col_name": Value::Text("hi"))'.
  named_params: NamedParams,

  /// List of columns that are targeted by the params. Useful for building Insert/Update queries.
  ///
  /// NOTE: This is a super-set of all columns and also includes the file_col_names below.
  /// NOTE: We could also infer them from placeholder names by stripping the leading ":".
  col_names: Vec<String>,

  /// List of files and contents to be written to an object store.
  files: FileMetadataContents,
  /// Subset of `col_names` containing only file columns. Useful for building Update/Delete queries
  /// to remove the files from the object store afterwards.
  file_col_names: Vec<String>,
}

impl Params {
  /// Converts a top-level Json object into trailbase_sqlite::Values and extract files.
  ///
  /// Note: that this function by design is non-recursive, since we're mapping to a flat hierarchy
  /// in sqlite, since even JSON/JSONB is simply text/blob that is lazily parsed.
  ///
  /// The expected format is:
  ///
  /// request = {
  ///   "col0": "text",
  ///   "col1": <base64(b"123")>,
  ///   "file_col": {
  ///     data: ...
  ///   }
  /// }
  ///
  /// The optional files parameter is there to receive files in case the input JSON was extracted
  /// form a multipart/form. In that case files are handled separately and not embedded in the JSON
  /// value itself in contrast to when the original request was an actual JSON request.
  pub fn from(
    metadata: &TableMetadata,
    json: JsonRow,
    multipart_files: Option<Vec<FileUploadInput>>,
  ) -> Result<Self, ParamsError> {
    let mut params = Params {
      table_name: metadata.name().to_string(),
      ..Default::default()
    };

    for (key, value) in json {
      // We simply skip unknown columns, this could simply be malformed input or version skew. This
      // is similar in spirit to protobuf's unknown fields behavior.
      let Some((col, col_meta)) = Self::column_by_name(metadata, &key) else {
        continue;
      };

      let (param, mut json_files) = extract_params_and_files_from_json(col, col_meta, value)?;
      if let Some(json_files) = json_files.as_mut() {
        // Note: files provided as a multipart form upload are handled below. They need more
        // special handling to establish the field.name to column mapping.
        params.files.append(json_files);
        params.file_col_names.push(key.to_string());
      }

      params.push_param(key, param);
    }

    // Note: files provided as part of a JSON request are handled above.
    if let Some(multipart_files) = multipart_files {
      params.append_multipart_files(metadata, multipart_files)?;
    }

    return Ok(params);
  }

  #[cfg(debug_assertions)]
  #[inline]
  fn column_by_name<'a>(
    metadata: &'a TableMetadata,
    field_name: &str,
  ) -> Option<(&'a Column, &'a ColumnMetadata)> {
    let Some(col) = metadata.column_by_name(field_name) else {
      info!(
        "Skipping field '{field_name}' in request: no matching column. This is just an FYI in dev builds and not an issue."
      );
      return None;
    };
    return Some(col);
  }

  #[cfg(not(debug_assertions))]
  #[inline]
  fn column_by_name<'a>(
    metadata: &'a TableMetadata,
    field_name: &str,
  ) -> Option<(&'a Column, &'a ColumnMetadata)> {
    return metadata.column_by_name(field_name);
  }

  #[inline]
  fn prefix_colon(s: &str) -> String {
    let mut new = String::with_capacity(s.len() + 1);
    new.push(':');
    new.push_str(s);
    return new;
  }

  pub fn push_param(&mut self, col: String, value: Value) {
    self
      .named_params
      .push((Self::prefix_colon(&col).into(), value));
    self.col_names.push(col);
  }

  pub(crate) fn column_names(&self) -> &Vec<String> {
    return &self.col_names;
  }

  pub(crate) fn named_params(&self) -> &NamedParams {
    &self.named_params
  }

  pub(crate) fn placeholders(&self) -> String {
    return self.named_params.iter().map(|(k, _v)| k).join(", ");
  }

  fn append_multipart_files(
    &mut self,
    metadata: &TableMetadata,
    multipart_files: Vec<FileUploadInput>,
  ) -> Result<(), ParamsError> {
    let files: Vec<(String, FileUpload, Vec<u8>)> = multipart_files
      .into_iter()
      .map(|file| {
        let (col_name, metadata, content) = file.consume()?;
        return match col_name {
          Some(col_name) => Ok((col_name, metadata, content)),
          None => Err(ParamsError::Column(
            "Multipart form upload missing name property",
          )),
        };
      })
      .collect::<Result<_, ParamsError>>()?;

    // Validate and organize by type;
    let mut uploaded_files = HashSet::<&'static str>::new();
    for (field_name, file_metadata, _content) in &files {
      // We simply skip unknown columns, this could simply be malformed input or version skew. This
      // is similar in spirit to protobuf's unknown fields behavior.
      let Some((col, col_meta)) = Self::column_by_name(metadata, field_name) else {
        continue;
      };

      let Some(JsonColumnMetadata::SchemaName(schema_name)) = &col_meta.json else {
        return Err(ParamsError::Column("Expected json column"));
      };

      let value = Value::Text(serde_json::to_string(&file_metadata)?);
      match schema_name.as_str() {
        "std.FileUpload" => {
          if !uploaded_files.insert(&col.name) {
            return Err(ParamsError::Column(
              "Collision: too many files for std.FileUpload",
            ));
          }

          self
            .named_params
            .push((Self::prefix_colon(&col.name).into(), value));
          self.col_names.push(col.name.to_string());
          self.file_col_names.push(col.name.to_string());
        }
        "std.FileUploads" => {
          self
            .named_params
            .push((Self::prefix_colon(&col.name).into(), value));
          self.col_names.push(col.name.to_string());
          self.file_col_names.push(col.name.to_string());
        }
        _ => {
          return Err(ParamsError::Column("Mismatching JSON schema"));
        }
      }
    }

    self.files.append(
      &mut files
        .into_iter()
        .map(|(_, metadata, content)| (metadata, content))
        .collect(),
    );

    return Ok(());
  }
}

pub(crate) struct Expansions {
  /// Contains the indexes on where to cut the resulting Row.
  ///
  /// The joins will lead to a row schema that looks something like:
  ///   (root_table..., foreign_table0..., foreign_table1...).
  pub indexes: Vec<(usize, Arc<TableMetadata>)>,
  /// The actual join statements.
  pub joins: Vec<String>,

  /// Select clauses in case the joins are aliased, i.e. a `prefix` is given.
  pub selects: Option<Vec<String>>,
}

impl Expansions {
  pub(crate) fn build<T: AsRef<str>>(
    table_metadata: &TableMetadataCache,
    table_name: &str,
    expand: &[T],
    prefix: Option<&str>,
  ) -> Result<Expansions, RecordError> {
    let Some(root_table) = table_metadata.get(table_name) else {
      return Err(RecordError::ApiRequiresTable);
    };

    let mut joins = vec![];
    let mut indexes = vec![(root_table.schema.columns.len(), root_table.clone())];

    for (idx, col_name) in expand.iter().enumerate() {
      if col_name.as_ref().is_empty() {
        continue;
      }
      let Some((column, _col_metadata)) = root_table.column_by_name(col_name.as_ref()) else {
        return Err(RecordError::ApiRequiresTable);
      };

      let Some(ColumnOption::ForeignKey {
        foreign_table: foreign_table_name,
        referred_columns: _,
        ..
      }) = column
        .options
        .iter()
        .find_or_first(|o| matches!(o, ColumnOption::ForeignKey { .. }))
      else {
        return Err(RecordError::ApiRequiresTable);
      };

      let Some(foreign_table) = table_metadata.get(foreign_table_name) else {
        return Err(RecordError::ApiRequiresTable);
      };

      let Some(foreign_pk_column_idx) = foreign_table.record_pk_column else {
        return Err(RecordError::ApiRequiresTable);
      };

      let foreign_pk_column = &foreign_table.schema.columns[foreign_pk_column_idx].name;

      // TODO: Check that `referred_columns` and foreign_pk_column are the same. It's already
      // validated as part of config validation.

      joins.push(format!(
        r#"LEFT JOIN "{foreign_table_name}" AS F{idx} ON {col_name} = F{idx}.{foreign_pk_column}"#,
        col_name = prefix.map_or_else(
          || col_name.as_ref().to_owned(),
          |p| format!("{p}.{}", col_name.as_ref())
        ),
      ));
      indexes.push((foreign_table.schema.columns.len(), foreign_table));
    }

    let selects = if prefix.is_none() {
      None
    } else {
      Some(
        (0..joins.len())
          .map(|idx| format!("F{idx}.*"))
          .collect::<Vec<_>>(),
      )
    };

    return Ok(Expansions {
      indexes,
      joins,
      selects,
    });
  }
}

pub(crate) struct SelectQueryBuilder;

impl SelectQueryBuilder {
  pub(crate) async fn run(
    state: &AppState,
    table_name: &str,
    pk_column: &str,
    pk_value: Value,
  ) -> Result<Option<trailbase_sqlite::Row>, trailbase_sqlite::Error> {
    return state
      .conn()
      .query_row(
        &format!(r#"SELECT * FROM "{table_name}" WHERE "{pk_column}" = $1"#),
        [pk_value],
      )
      .await;
  }

  pub(crate) async fn run_expanded(
    state: &AppState,
    table_name: &str,
    pk_column: &str,
    pk_value: Value,
    expand: &[&str],
  ) -> Result<Vec<(Arc<TableMetadata>, trailbase_sqlite::Row)>, RecordError> {
    let table_metadata = state.table_metadata();
    let Expansions { indexes, joins, .. } =
      Expansions::build(table_metadata, table_name, expand, None)?;

    let sql = format!(
      r#"SELECT * FROM "{table_name}" AS R {} WHERE R.{pk_column} = $1"#,
      joins.join(" ")
    );

    let Some(row) = state.conn().query_row(&sql, [pk_value]).await? else {
      return Ok(vec![]);
    };

    let mut curr = row;
    let mut result = Vec::with_capacity(indexes.len());
    for (idx, metadata) in indexes {
      let next = curr.split_off(idx);
      result.push((metadata, curr));
      curr = next;
    }

    return Ok(result);
  }
}

pub(crate) struct GetFileQueryBuilder;

impl GetFileQueryBuilder {
  pub(crate) async fn run(
    state: &AppState,
    table_name: &str,
    file_column: (&Column, &ColumnMetadata),
    pk_column: &str,
    pk_value: Value,
  ) -> Result<FileUpload, QueryError> {
    return match &file_column.1.json {
      Some(JsonColumnMetadata::SchemaName(name)) if name == "std.FileUpload" => {
        let column_name = &file_column.0.name;

        let Some(row) = state
          .conn()
          .query_row(
            &format!(r#"SELECT "{column_name}" FROM "{table_name}" WHERE "{pk_column}" = $1"#),
            [pk_value],
          )
          .await?
        else {
          return Err(QueryError::NotFound);
        };

        let json: String = row.get(0)?;
        let file_upload: FileUpload = serde_json::from_str(&json)?;
        Ok(file_upload)
      }
      _ => Err(QueryError::Precondition("Not a file")),
    };
  }
}

pub(crate) struct GetFilesQueryBuilder;

impl GetFilesQueryBuilder {
  pub(crate) async fn run(
    state: &AppState,
    table_name: &str,
    file_column: (&Column, &ColumnMetadata),
    pk_column: &str,
    pk_value: Value,
  ) -> Result<FileUploads, QueryError> {
    return match &file_column.1.json {
      Some(JsonColumnMetadata::SchemaName(name)) if name == "std.FileUploads" => {
        let column_name = &file_column.0.name;

        let Some(row) = state
          .conn()
          .query_row(
            &format!(r#"SELECT "{column_name}" FROM "{table_name}" WHERE "{pk_column}" = $1"#),
            [pk_value],
          )
          .await?
        else {
          return Err(QueryError::NotFound);
        };

        let contents: String = row.get(0)?;
        let file_uploads: FileUploads = serde_json::from_str(&contents)?;
        Ok(file_uploads)
      }
      _ => Err(QueryError::Precondition("Not a files list")),
    };
  }
}

pub(crate) struct InsertQueryBuilder;

impl InsertQueryBuilder {
  pub(crate) async fn run<T: Send + 'static>(
    state: &AppState,
    params: Params,
    conflict_resolution: Option<ConflictResolutionStrategy>,
    return_column_name: Option<&str>,
    extractor: impl Fn(&rusqlite::Row) -> Result<T, trailbase_sqlite::Error> + Send + 'static,
  ) -> Result<T, QueryError> {
    let (query, named_params, mut files) =
      Self::build_insert_query(params, conflict_resolution, return_column_name)?;

    // We're storing any files to the object store first to make sure the DB entry is valid right
    // after commit and not racily pointing to soon-to-be-written files.
    if !files.is_empty() {
      let objectstore = state.objectstore();
      for (metadata, content) in &mut files {
        write_file(objectstore, metadata, content).await?;
      }
    }

    let result = state
      .conn()
      .call(move |conn| {
        let mut stmt = conn.prepare(&query)?;
        named_params.bind(&mut stmt)?;
        let mut result = stmt.raw_query();

        return match result.next()? {
          Some(row) => Ok(extractor(row)?),
          _ => Err(rusqlite::Error::QueryReturnedNoRows.into()),
        };
      })
      .await;

    if result.is_err() && !files.is_empty() {
      let objectstore = state.objectstore();

      for (metadata, _files) in &files {
        let path = object_store::path::Path::from(metadata.path());
        if let Err(err) = objectstore.delete(&path).await {
          warn!("Failed to cleanup file after failed insertion (leak): {err}");
        }
      }
    }

    return Ok(result?);
  }

  pub(crate) async fn run_bulk<T: Send + 'static>(
    state: &AppState,
    params_list: Vec<Params>,
    conflict_resolution: Option<ConflictResolutionStrategy>,
    return_column_name: Option<&str>,
    extractor: impl Fn(&rusqlite::Row) -> Result<T, trailbase_sqlite::Error> + Send + 'static,
  ) -> Result<Vec<T>, QueryError> {
    let mut all_files: FileMetadataContents = vec![];
    let mut query_and_params: Vec<(String, NamedParams)> = vec![];

    for params in params_list {
      let (query, named_params, mut files) =
        Self::build_insert_query(params, conflict_resolution, return_column_name)?;

      all_files.append(&mut files);
      query_and_params.push((query, named_params));
    }

    // We're storing any files to the object store first to make sure the DB entry is valid right
    // after commit and not racily pointing to soon-to-be-written files.
    if !all_files.is_empty() {
      let objectstore = state.objectstore();
      for (metadata, content) in &mut all_files {
        write_file(objectstore, metadata, content).await?;
      }
    }

    let result = state
      .conn()
      .call(move |conn| {
        let mut rows = Vec::<T>::with_capacity(query_and_params.len());

        let tx = conn.transaction()?;

        for (query, named_params) in query_and_params {
          let mut stmt = tx.prepare(&query)?;
          named_params.bind(&mut stmt)?;
          let mut result = stmt.raw_query();

          match result.next()? {
            Some(row) => rows.push(extractor(row)?),
            _ => {
              return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
          };
        }

        tx.commit()?;

        return Ok(rows);
      })
      .await;

    if result.is_err() && !all_files.is_empty() {
      let objectstore = state.objectstore();

      for (metadata, _files) in &all_files {
        let path = object_store::path::Path::from(metadata.path());
        if let Err(err) = objectstore.delete(&path).await {
          warn!("Failed to cleanup file after failed insertion (leak): {err}");
        }
      }
    }

    return Ok(result?);
  }

  fn build_insert_query(
    params: Params,
    conflict_resolution: Option<ConflictResolutionStrategy>,
    return_column_name: Option<&str>,
  ) -> Result<(String, NamedParams, FileMetadataContents), QueryError> {
    let table_name = &params.table_name;

    let conflict_clause = Self::conflict_resolution_clause(
      conflict_resolution.unwrap_or(ConflictResolutionStrategy::Undefined),
    );

    let return_fragment: Cow<'_, str> = match return_column_name {
      Some(return_column_name) => {
        if return_column_name == "*" {
          Cow::Borrowed(r#"RETURNING *"#)
        } else {
          format!(r#"RETURNING "{return_column_name}""#).into()
        }
      }
      None => Cow::Borrowed(""),
    };

    let column_names = params.column_names();
    let query = if !column_names.is_empty() {
      format!(
        r#"INSERT {conflict_clause} INTO "{table_name}" ({col_names}) VALUES ({placeholders}) {return_fragment}"#,
        col_names = Self::build_col_names(column_names),
        placeholders = params.placeholders(),
      )
    } else {
      // The insert empty record case, i.e. "{}".
      format!(r#"INSERT {conflict_clause} INTO "{table_name}" DEFAULT VALUES {return_fragment}"#)
    };

    return Ok((query, params.named_params, params.files));
  }

  #[inline]
  fn build_col_names(column_names: &[String]) -> String {
    let mut s = String::new();
    for (i, name) in column_names.iter().enumerate() {
      if i > 0 {
        s.push_str(", \"");
      } else {
        s.push('"');
      }
      s.push_str(name);
      s.push('"');
    }
    return s;
  }

  #[inline]
  fn conflict_resolution_clause(config: ConflictResolutionStrategy) -> &'static str {
    type C = ConflictResolutionStrategy;
    return match config {
      C::Undefined => "",
      C::Abort => "OR ABORT",
      C::Rollback => "OR ROLLBACK",
      C::Fail => "OR FAIL",
      C::Ignore => "OR IGNORE",
      C::Replace => "OR REPLACE",
    };
  }
}

pub(crate) struct UpdateQueryBuilder;

impl UpdateQueryBuilder {
  pub(crate) async fn run(
    state: &AppState,
    metadata: &TableMetadata,
    mut params: Params,
    pk_column: &str,
    pk_value: Value,
  ) -> Result<(), QueryError> {
    let table_name = metadata.name();
    assert_eq!(params.table_name, *table_name);
    if params.column_names().is_empty() {
      return Ok(());
    }

    params.push_param(pk_column.to_string(), pk_value.clone());

    // We're storing to object store before writing the entry to the DB.
    let mut files = std::mem::take(&mut params.files);
    if !files.is_empty() {
      let objectstore = state.objectstore();
      for (metadata, content) in &mut files {
        write_file(objectstore, metadata, content).await?;
      }
    }

    async fn row_update(
      conn: &trailbase_sqlite::Connection,
      table_name: &str,
      params: Params,
      pk_column: &str,
      pk_value: Value,
    ) -> Result<Option<trailbase_sqlite::Row>, QueryError> {
      let setters: String = {
        assert_eq!(params.col_names.len(), params.named_params.len());

        std::iter::zip(&params.col_names, &params.named_params)
          .map(|(col_name, (placeholder, _value))| format!(r#""{col_name}" = {placeholder}"#))
          .join(", ")
      };

      let pk_column = pk_column.to_string();
      let table_name = table_name.to_string();
      let files_row = conn
        .call(move |conn| {
          let tx = conn.transaction()?;

          // First, fetch updated file column contents so we can delete the files after updating the
          // column.
          let files_row = if params.file_col_names.is_empty() {
            None
          } else {
            let file_columns = params.file_col_names.join(", ");

            let mut stmt = tx.prepare(&format!(
              r#"SELECT {file_columns} FROM "{table_name}" WHERE "{pk_column}" = :{pk_column}"#
            ))?;

            use trailbase_sqlite::Params;
            [(":pk_column", pk_value)].bind(&mut stmt)?;

            let mut rows = stmt.raw_query();
            if let Some(row) = rows.next()? {
              Some(trailbase_sqlite::Row::from_row(row, None)?)
            } else {
              None
            }
          };

          // Update the column.
          {
            let mut stmt = tx.prepare(&format!(
              r#"UPDATE "{table_name}" SET {setters} WHERE "{pk_column}" = :{pk_column}"#
            ))?;
            use trailbase_sqlite::Params;
            params.named_params.bind(&mut stmt)?;

            stmt.raw_execute()?;
          }

          tx.commit()?;

          return Ok(files_row);
        })
        .await?;

      return Ok(files_row);
    }

    let files_row = match row_update(state.conn(), table_name, params, pk_column, pk_value).await {
      Ok(files_row) => files_row,
      Err(err) => {
        if !files.is_empty() {
          let store = state.objectstore();
          for (metadata, _content) in &files {
            let path = object_store::path::Path::from(metadata.path());
            if let Err(err) = store.delete(&path).await {
              warn!("Failed to cleanup file after failed insertion (leak): {err}");
            }
          }
        }

        return Err(err);
      }
    };

    // Finally, if everything else went well delete files from columns that were updated and are no
    // longer referenced.
    if let Some(files_row) = files_row {
      delete_files_in_row(state, metadata, files_row).await?;
    }

    return Ok(());
  }
}

pub(crate) struct DeleteQueryBuilder;

impl DeleteQueryBuilder {
  pub(crate) async fn run(
    state: &AppState,
    metadata: &TableMetadata,
    pk_column: &str,
    pk_value: Value,
  ) -> Result<(), QueryError> {
    let table_name = metadata.name();

    let row = state
      .conn()
      .query_row(
        &format!(r#"DELETE FROM "{table_name}" WHERE "{pk_column}" = $1 RETURNING *"#),
        [pk_value],
      )
      .await?
      .ok_or_else(|| QueryError::Sql(rusqlite::Error::QueryReturnedNoRows.into()))?;

    // Finally, delete files.
    delete_files_in_row(state, metadata, row).await?;

    return Ok(());
  }
}

async fn write_file(
  store: &dyn ObjectStore,
  metadata: &FileUpload,
  data: &mut Vec<u8>,
) -> Result<(), object_store::Error> {
  let path = object_store::path::Path::from(metadata.path());

  let mut writer = store.put_multipart(&path).await?;
  writer.put_part(std::mem::take(data).into()).await?;
  writer.complete().await?;

  return Ok(());
}

fn try_json_array_to_blob(arr: &Vec<serde_json::Value>) -> Result<Value, ParamsError> {
  let mut byte_array: Vec<u8> = vec![];
  for el in arr {
    match el {
      serde_json::Value::Number(num) => {
        let Some(int) = num.as_i64() else {
          return Err(ParamsError::UnexpectedType(
            "NonByteNumber",
            format!("Number type: {num:?}"),
          ));
        };

        let Ok(byte) = int.try_into() else {
          return Err(ParamsError::UnexpectedType(
            "NonByteNumber",
            format!("Out-of-range int: {int}"),
          ));
        };

        byte_array.push(byte);
      }
      x => {
        return Err(ParamsError::InhomogenousArray(format!(
          "Expected number, got {x:?}"
        )));
      }
    };
  }

  return Ok(Value::Blob(byte_array));
}

pub(crate) fn json_string_to_value(
  data_type: ColumnDataType,
  value: String,
) -> Result<Value, ParamsError> {
  return Ok(match data_type {
    ColumnDataType::Null => Value::Null,
    // Strict/storage types
    ColumnDataType::Any => Value::Text(value),
    ColumnDataType::Text => Value::Text(value),
    ColumnDataType::Blob => Value::Blob(match (value.len(), value) {
      // Special handling for text encoded UUIDs. Right now we're guessing based on length, it
      // would be more explicit rely on CHECK(...) column options.
      // NOTE: That uuids also parse as url-safe base64, that's why we treat it as a fall-first.
      (36, v) => uuid::Uuid::parse_str(&v)
        .map(|v| v.into())
        .or_else(|_| BASE64_URL_SAFE.decode(&v))?,
      (_, v) => BASE64_URL_SAFE.decode(&v)?,
    }),
    ColumnDataType::Integer => Value::Integer(value.parse::<i64>()?),
    ColumnDataType::Real => Value::Real(value.parse::<f64>()?),
    ColumnDataType::Numeric => Value::Integer(value.parse::<i64>()?),
    // JSON types.
    ColumnDataType::JSONB => Value::Blob(value.into_bytes().to_vec()),
    ColumnDataType::JSON => Value::Text(value),
    // Affine types
    //
    // Integers:
    ColumnDataType::Int
    | ColumnDataType::TinyInt
    | ColumnDataType::SmallInt
    | ColumnDataType::MediumInt
    | ColumnDataType::BigInt
    | ColumnDataType::UnignedBigInt
    | ColumnDataType::Int2
    | ColumnDataType::Int4
    | ColumnDataType::Int8 => Value::Integer(value.parse::<i64>()?),
    // Text:
    ColumnDataType::Character
    | ColumnDataType::Varchar
    | ColumnDataType::VaryingCharacter
    | ColumnDataType::NChar
    | ColumnDataType::NativeCharacter
    | ColumnDataType::NVarChar
    | ColumnDataType::Clob => Value::Text(value),
    // Real:
    ColumnDataType::Double | ColumnDataType::DoublePrecision | ColumnDataType::Float => {
      Value::Real(value.parse::<f64>()?)
    }
    // Numeric
    ColumnDataType::Boolean
    | ColumnDataType::Decimal
    | ColumnDataType::Date
    | ColumnDataType::DateTime => Value::Integer(value.parse::<i64>()?),
  });
}

pub fn simple_json_value_to_param(
  col_type: ColumnDataType,
  value: serde_json::Value,
) -> Result<Value, ParamsError> {
  let param = match value {
    serde_json::Value::Object(ref _map) => {
      return Err(ParamsError::UnexpectedType(
        "Object",
        format!("Trivial type: {col_type:?}"),
      ));
    }
    serde_json::Value::Array(ref arr) => {
      // NOTE: Convert Array<number> to Blob. Note, we also support blobs as base64 which are
      // handled below in the string  case.
      if col_type != ColumnDataType::Blob {
        return Err(ParamsError::UnexpectedType(
          "Array",
          format!("Trivial type: {col_type:?}"),
        ));
      }

      try_json_array_to_blob(arr)?
    }
    serde_json::Value::Null => Value::Null,
    serde_json::Value::Bool(b) => Value::Integer(b as i64),
    serde_json::Value::String(str) => json_string_to_value(col_type, str)?,
    serde_json::Value::Number(number) => {
      if let Some(n) = number.as_i64() {
        Value::Integer(n)
      } else if let Some(n) = number.as_u64() {
        Value::Integer(n as i64)
      } else if let Some(n) = number.as_f64() {
        Value::Real(n)
      } else {
        warn!("Not a valid number: {number:?}");
        return Err(ParamsError::NotANumber);
      }
    }
  };

  return Ok(param);
}

fn extract_params_and_files_from_json(
  col: &Column,
  col_meta: &ColumnMetadata,
  value: serde_json::Value,
) -> Result<(Value, Option<FileMetadataContents>), ParamsError> {
  let col_name = &col.name;
  match value {
    serde_json::Value::Object(ref _map) => {
      // Only text columns are allowed to store nested JSON as text.
      if col.data_type != ColumnDataType::Text {
        return Err(ParamsError::NestedObject(format!(
          "Column data mismatch for: {col_name}"
        )));
      }

      let Some(json) = &col_meta.json else {
        return Err(ParamsError::NestedObject(format!(
          "Plain text column w/o JSON: {col_name}"
        )));
      };

      // By default, nested json will be serialized to text since that's what sqlite expected.
      // For FileUpload columns we have special handling to extract the actual payload and
      // convert the FileUploadInput into an actual FileUpload schema json.
      match json {
        JsonColumnMetadata::SchemaName(name) if name == "std.FileUpload" => {
          let file_upload: FileUploadInput = serde_json::from_value(value)?;

          let (_col_name, metadata, content) = file_upload.consume()?;
          let param = Value::Text(serde_json::to_string(&metadata)?);

          return Ok((param, Some(vec![(metadata, content)])));
        }
        _ => {
          json.validate(&value)?;
          return Ok((Value::Text(value.to_string()), None));
        }
      }
    }
    serde_json::Value::Array(ref arr) => {
      // If the we're building a Param for a schema column, unpack the json (and potentially files)
      // and validate it.
      match col.data_type {
        ColumnDataType::Blob => return Ok((try_json_array_to_blob(arr)?, None)),
        ColumnDataType::Text => {
          if let Some(ref json) = col_meta.json {
            match json {
              JsonColumnMetadata::SchemaName(name) if name == "std.FileUploads" => {
                let file_upload_vec: Vec<FileUploadInput> = serde_json::from_value(value)?;

                // TODO: Optimize the copying here. Not very critical.
                let mut temp: Vec<FileUpload> = vec![];
                let mut uploads: FileMetadataContents = vec![];
                for file in file_upload_vec {
                  let (_col_name, metadata, content) = file.consume()?;
                  temp.push(metadata.clone());
                  uploads.push((metadata, content));
                }

                let param = Value::Text(serde_json::to_string(&FileUploads(temp))?);

                return Ok((param, Some(uploads)));
              }
              schema => {
                schema.validate(&value)?;
                return Ok((Value::Text(value.to_string()), None));
              }
            }
          }
        }
        _ => {}
      }

      return Err(ParamsError::NestedArray(format!(
        "Received nested array for unsuitable column: {col_name}"
      )));
    }
    x => return Ok((simple_json_value_to_param(col.data_type, x)?, None)),
  };
}

/// A lazy representation of SQL query parameters derived from the request json to shared between
/// handler and the policy engine.
///
/// If the request gets rejected by the policy we want to avoid parsing the request JSON and if the
/// engine requires a parse we don't want to re-parse in the handler.
///
/// NOTE: Table level access checking could probably happen even sooner before we process multipart
/// streams at all.
pub struct LazyParams<'a> {
  // Input
  json_row: JsonRow,
  metadata: &'a TableMetadata,
  multipart_files: Option<Vec<FileUploadInput>>,

  // Output
  params: Option<Result<Params, ParamsError>>,
}

impl<'a> LazyParams<'a> {
  pub fn new(
    metadata: &'a TableMetadata,
    json_row: JsonRow,
    multipart_files: Option<Vec<FileUploadInput>>,
  ) -> Self {
    LazyParams {
      json_row,
      metadata,
      multipart_files,
      params: None,
    }
  }

  pub fn params(&mut self) -> Result<&'_ Params, ParamsError> {
    if let Some(ref params) = self.params {
      return params.as_ref().map_err(|err| err.clone());
    }

    let json_row = std::mem::take(&mut self.json_row);
    let multipart_files = std::mem::take(&mut self.multipart_files);

    let params = self
      .params
      .insert(Params::from(self.metadata, json_row, multipart_files));
    return params.as_ref().map_err(|err| err.clone());
  }

  pub fn consume(self) -> Result<Params, ParamsError> {
    if let Some(params) = self.params {
      return params;
    }
    return Params::from(self.metadata, self.json_row, self.multipart_files);
  }
}

#[cfg(test)]
mod tests {
  use base64::prelude::*;
  use schemars::{schema_for, JsonSchema};
  use serde_json::json;

  use super::*;
  use crate::records::test_utils::json_row_from_value;
  use crate::schema::Table;
  use crate::table_metadata::{sqlite3_parse_into_statement, TableMetadata};
  use crate::util::id_to_b64;

  #[tokio::test]
  async fn test_json_to_sql() {
    #[allow(unused)]
    #[derive(JsonSchema)]
    struct TestSchema {
      text: String,
      array: Option<Vec<serde_json::Value>>,
      blob: Option<Vec<u8>>,
    }

    const SCHEMA_NAME: &str = "test.TestSchema";
    let schema = schema_for!(TestSchema);
    const ID_COL: &str = "myid";
    const ID_COL_PLACEHOLDER: &str = ":myid";

    let sql = format!(
      r#"
          CREATE TABLE user (
            {ID_COL} BLOB NOT NULL,
            blob BLOB NOT NULL,
            text TEXT NOT NULL,
            json_col TEXT NOT NULL CHECK(jsonschema('{SCHEMA_NAME}', json_col)),
            num INTEGER NOT NULL DEFAULT 42,
            real REAL NOT NULL DEFAULT 23.0
          )
    "#
    );

    let table: Table = sqlite3_parse_into_statement(&sql)
      .unwrap()
      .unwrap()
      .try_into()
      .unwrap();

    trailbase_sqlite::schema::set_user_schema(
      SCHEMA_NAME,
      Some(serde_json::to_value(schema).unwrap()),
    )
    .unwrap();
    trailbase_extension::jsonschema::get_schema(SCHEMA_NAME).unwrap();

    let metadata = TableMetadata::new(table.clone(), &[table]);

    let id: [u8; 16] = uuid::Uuid::now_v7().as_bytes().clone();
    let blob: Vec<u8> = [0; 128].to_vec();
    let text = "some text :)";
    let num = 5;
    let real = 3.0;

    let assert_params = |p: Params| {
      assert!(p.named_params.len() >= 5, "{:?}", p.named_params);

      for (param, value) in &p.named_params {
        match param.as_ref() {
          ID_COL_PLACEHOLDER => {
            assert!(
              matches!(value, Value::Blob(x) if *x == id),
              "VALUE: {value:?}"
            );
          }
          ":blob" => {
            assert!(matches!(value, Value::Blob(x) if *x == blob));
          }
          ":text" => {
            assert!(matches!(value, Value::Text(x) if x.contains("some text :)")));
          }
          ":num" => {
            assert!(matches!(value, Value::Integer(x) if *x == 5));
          }
          ":real" => {
            assert!(matches!(value, Value::Real(x) if *x == 3.0));
          }
          ":json_col" => {
            assert!(matches!(value, Value::Text(_x)));
          }
          x => assert!(false, "{x}"),
        }
      }
    };

    {
      // Test that blob columns can be passed as base64.
      let value = json!({
        ID_COL: id_to_b64(&id),
        "blob": BASE64_URL_SAFE.encode(&blob),
        "text": text,
        "num": num,
        "real": real,
      });

      assert_params(Params::from(&metadata, json_row_from_value(value).unwrap(), None).unwrap());
    }

    {
      // Test that blob columns can be passed as int array and numbers can be passed as string.
      let value = json!({
        ID_COL: id,
        "blob": blob,
        "text": text,
        "num": "5",
        "real": "3",
      });

      assert_params(Params::from(&metadata, json_row_from_value(value).unwrap(), None).unwrap());
    }

    {
      let value = json!({
        ID_COL: id,
        "blob": blob,
        "text": json!({
          "email": text,
        }),
        "num": "5",
        "real": "3",
      });

      assert!(Params::from(&metadata, json_row_from_value(value).unwrap(), None).is_err());

      // Test that nested JSON object can be passed.
      let value = json!({
        ID_COL: id,
        "blob": blob,
        "text": text,
        "json_col": json!({
          "text": text,
        }),
        "num": "5",
        "real": "3",
      });

      let params = Params::from(&metadata, json_row_from_value(value).unwrap(), None).unwrap();
      assert_params(params);
    }

    {
      let value = json!({
        ID_COL: id,
        "blob": blob,
        "text": json!([text, 1,2,3,4, "foo"]),
        "num": "5",
        "real": "3",
      });

      assert!(Params::from(&metadata, json_row_from_value(value).unwrap(), None).is_err());

      // Test that nested JSON array can be passed.
      let nested_json_blob: Vec<u8> = vec![65, 66, 67, 68];
      let value = json!({
        ID_COL: id,
        "blob": blob,
        "text": text,
        "json_col": json!({
          "text": "test",
          "array": [text, 1,2,3,4, "foo"],
          "blob": nested_json_blob,
        }),
        "num": "5",
        "real": "3",
      });

      let params = Params::from(&metadata, json_row_from_value(value).unwrap(), None).unwrap();

      let json_col: Vec<Value> = params
        .named_params
        .iter()
        .filter_map(|(name, value)| {
          if name == ":json_col" {
            return Some(value.clone());
          }
          return None;
        })
        .collect();

      assert_eq!(json_col.len(), 1);
      let Value::Text(ref text) = json_col[0] else {
        panic!("Unexpected param type: {:?}", json_col[0]);
      };

      // Test encoded nested json against golden.
      assert_eq!(
        serde_json::from_str::<serde_json::Value>(text).unwrap(),
        serde_json::json!({
          "array": Vec::<serde_json::Value>::from(["some text :)".into(),1.into(),2.into(),3.into(),4.into(),"foo".into()]),
          "blob": [65,66,67,68],
          "text": "test",
        }),
      );

      assert_params(params);
    }
  }

  #[tokio::test]
  async fn test_json_string_to_value() {
    let conn = trailbase_sqlite::Connection::open_in_memory().unwrap();
    conn
      .execute(
        r#"CREATE TABLE test (
        id    BLOB NOT NULL,
        text  TEXT
    )"#,
        (),
      )
      .await
      .unwrap();

    let id_string = "01950408-de17-7f13-8ef5-66d90b890bfd";
    let id = uuid::Uuid::parse_str(id_string).unwrap();

    conn
      .execute(
        "INSERT INTO test (id, text) VALUES ($1, $2);",
        trailbase_sqlite::params!(id.into_bytes(), "mytext",),
      )
      .await
      .unwrap();

    let value = json_string_to_value(ColumnDataType::Blob, id_string.to_string()).unwrap();
    let blob = match value {
      rusqlite::types::Value::Blob(ref blob) => blob.clone(),
      _ => panic!("Not a blob"),
    };

    assert_eq!(
      blob.len(),
      16,
      "Blob: {value:?} {}",
      String::from_utf8_lossy(&blob)
    );
    assert_eq!(uuid::Uuid::from_slice(&blob).unwrap(), id);

    let rows = conn
      .query("SELECT * FROM test WHERE id = $1", [value])
      .await
      .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<String>(1).unwrap(), "mytext");
  }
}
