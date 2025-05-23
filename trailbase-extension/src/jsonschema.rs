use jsonschema::Validator;
use mini_moka::sync::Cache;
use parking_lot::Mutex;
use rusqlite::Error;
use rusqlite::functions::Context;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

// NOTE:: Validation error is very large, we thus Box it.
pub type ValidationError = Box<jsonschema::ValidationError<'static>>;

type CustomValidatorFn = Arc<dyn Fn(&serde_json::Value, Option<&str>) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct SchemaEntry {
  schema: serde_json::Value,
  validator: Arc<Validator>,
  custom_validator: Option<CustomValidatorFn>,
}

impl SchemaEntry {
  pub fn from(
    schema: serde_json::Value,
    custom_validator: Option<CustomValidatorFn>,
  ) -> Result<Self, ValidationError> {
    let validator = Validator::new(&schema)?;

    return Ok(Self {
      schema,
      validator: validator.into(),
      custom_validator,
    });
  }
}

static SCHEMA_REGISTRY: LazyLock<Mutex<HashMap<String, SchemaEntry>>> =
  LazyLock::new(|| Mutex::new(HashMap::<String, SchemaEntry>::new()));

pub fn set_schemas(schema_entries: Option<Vec<(String, SchemaEntry)>>) {
  let mut lock = SCHEMA_REGISTRY.lock();
  lock.clear();

  if let Some(entries) = schema_entries {
    for (name, entry) in entries {
      lock.insert(name, entry);
    }
  }
}

pub fn set_schema(name: &str, entry: Option<SchemaEntry>) {
  if let Some(entry) = entry {
    SCHEMA_REGISTRY.lock().insert(name.to_string(), entry);
  } else {
    SCHEMA_REGISTRY.lock().remove(name);
  }
}

pub fn get_schema(name: &str) -> Option<serde_json::Value> {
  SCHEMA_REGISTRY.lock().get(name).map(|s| s.schema.clone())
}

pub fn get_compiled_schema(name: &str) -> Option<Arc<Validator>> {
  SCHEMA_REGISTRY
    .lock()
    .get(name)
    .map(|s| s.validator.clone())
}

pub fn get_schemas() -> Vec<(String, serde_json::Value)> {
  SCHEMA_REGISTRY
    .lock()
    .iter()
    .map(|(name, schema)| (name.clone(), schema.schema.clone()))
    .collect()
}

pub(crate) fn jsonschema_by_name(context: &Context) -> Result<bool, Error> {
  let schema_name = context.get_raw(0).as_str()?;

  // Get and parse the JSON contents. If it's invalid JSON to start with, there's not much
  // we can validate.
  let Some(contents) = context.get_raw(1).as_str_or_null()? else {
    return Ok(true);
  };

  let json = serde_json::from_str(contents)
    .map_err(|err| Error::UserFunctionError(format!("Invalid JSON: {contents} => {err}").into()))?;

  // Then get/build the schema validator for the given pattern.
  let Some(entry) = SCHEMA_REGISTRY.lock().get(schema_name).cloned() else {
    return Err(Error::UserFunctionError(
      format!("Schema {schema_name} not found").into(),
    ));
  };

  if !entry.validator.is_valid(&json) {
    return Ok(false);
  }

  if let Some(validator) = entry.custom_validator {
    if !validator(&json, None) {
      return Ok(false);
    }
  }

  return Ok(true);
}

pub(crate) fn jsonschema_by_name_with_extra_args(context: &Context) -> Result<bool, Error> {
  let schema_name = context.get_raw(0).as_str()?;
  let extra_args = context.get_raw(2).as_str()?;

  // Get and parse the JSON contents. If it's invalid JSON to start with, there's not much
  // we can validate.
  let Some(contents) = context.get_raw(1).as_str_or_null()? else {
    return Ok(true);
  };
  let json = serde_json::from_str(contents)
    .map_err(|err| Error::UserFunctionError(format!("Invalid JSON: {contents} => {err}").into()))?;

  // Then get/build the schema validator for the given pattern.
  let Some(entry) = SCHEMA_REGISTRY.lock().get(schema_name).cloned() else {
    return Err(Error::UserFunctionError(
      format!("Schema {schema_name} not found").into(),
    ));
  };

  if !entry.validator.is_valid(&json) {
    return Ok(false);
  }

  if let Some(validator) = entry.custom_validator {
    if !validator(&json, Some(extra_args)) {
      return Ok(false);
    }
  }

  return Ok(true);
}

static SCHEMA_CACHE: LazyLock<Cache<String, Arc<Validator>>> = LazyLock::new(|| Cache::new(256));

pub(crate) fn jsonschema_matches(context: &Context) -> Result<bool, Error> {
  // First, get and parse the JSON contents. If it's invalid JSON to start with, there's not much
  // we can validate.
  let Some(contents) = context.get_raw(1).as_str_or_null()? else {
    return Ok(true);
  };
  let json = serde_json::from_str(contents).map_err(|err| {
    Error::UserFunctionError(format!("Invalid JSON: '{contents}' => {err}").into())
  })?;

  let pattern = context.get_raw(0).as_str()?.to_string();

  // Then get/build the schema validator for the given pattern.
  let valid = match SCHEMA_CACHE.get(&pattern) {
    Some(validator) => validator.is_valid(&json),
    None => {
      let schema = serde_json::from_str(&pattern)
        .map_err(|err| Error::UserFunctionError(format!("Invalid JSON Schema: {err}").into()))?;
      let validator = Validator::new(&schema).map_err(|err| {
        Error::UserFunctionError(format!("Failed to compile Schema: {err}").into())
      })?;

      let valid = validator.is_valid(&json);
      SCHEMA_CACHE.insert(pattern, Arc::new(validator));
      valid
    }
  };

  return Ok(valid);
}

#[cfg(test)]
mod tests {
  use super::*;
  use rusqlite::params;

  #[test]
  fn test_explicit_jsonschema() {
    let conn = crate::connect_sqlite(None, None).unwrap();

    let text0_schema = r#"
        {
          "type": "object",
          "properties": {
            "name": { "type": "string" },
            "age": { "type": "integer", "minimum": 0 }
          },
          "required": ["name"]
        }
    "#;

    let text1_schema = r#"{ "type": "string" }"#;

    let create_table = format!(
      r#"
        CREATE TABLE test (
          text0    TEXT NOT NULL CHECK(jsonschema_matches('{text0_schema}', text0)),
          text1    TEXT NOT NULL CHECK(jsonschema_matches('{text1_schema}', text1))
        ) STRICT;
      "#
    );
    conn.execute(&create_table, ()).unwrap();

    {
      conn
        .execute(
          r#"INSERT INTO test (text0, text1) VALUES ('{"name": "foo"}', '"text"')"#,
          params!(),
        )
        .unwrap();
    }

    {
      assert!(
        conn
          .execute(
            r#"INSERT INTO test (text0, text1) VALUES ('{"name": "foo", "age": -5}', '"text"')"#,
            params!(),
          )
          .is_err()
      );
    }
  }

  #[test]
  fn test_registerd_jsonschema() {
    let conn = crate::connect_sqlite(None, None).unwrap();

    let text0_schema = r#"
        {
          "type": "object",
          "properties": {
            "name": { "type": "string" },
            "age": { "type": "integer", "minimum": 0 }
          },
          "required": ["name"]
        }
    "#;

    fn starts_with(v: &serde_json::Value, param: Option<&str>) -> bool {
      if let Some(param) = param {
        if let serde_json::Value::Object(map) = v {
          if let Some(serde_json::Value::String(str)) = map.get("name") {
            if str.starts_with(param) {
              return true;
            }
          }
        }
      }
      return false;
    }

    set_schema(
      "name0",
      Some(
        SchemaEntry::from(
          serde_json::from_str(text0_schema).unwrap(),
          Some(Arc::new(starts_with)),
        )
        .unwrap(),
      ),
    );

    let create_table = format!(
      r#"
        CREATE TABLE test (
          text0    TEXT NOT NULL CHECK(jsonschema('name0', text0, 'prefix'))
        ) STRICT;
      "#
    );
    conn.execute(&create_table, ()).unwrap();

    conn
      .execute(
        r#"INSERT INTO test (text0) VALUES ('{"name": "prefix_foo"}')"#,
        params!(),
      )
      .unwrap();

    assert!(
      conn
        .execute(
          r#"INSERT INTO test (text0) VALUES ('{"name": "WRONG_PREFIX_foo"}')"#,
          params!(),
        )
        .is_err()
    );
  }
}
