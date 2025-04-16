use axum::body::Body;
use axum::http::{header, Request};
use axum::response::Response;
use axum_client_ip::InsecureClientIp;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::field::Field;
use tracing::span::{Attributes, Id, Record, Span};
use tracing::Level;
use tracing_subscriber::layer::{Context, Layer};
use uuid::Uuid;

use crate::util::get_header;
use crate::AppState;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
enum HttpMethod {
  Unknown,
  Options,
  Get,
  Post,
  Put,
  Delete,
  Head,
  Trace,
  Connect,
  Patch,
}

impl HttpMethod {
  fn as_str(&self) -> &'static str {
    return match self {
      HttpMethod::Options => axum::http::Method::OPTIONS.as_str(),
      HttpMethod::Get => axum::http::Method::GET.as_str(),
      HttpMethod::Post => axum::http::Method::POST.as_str(),
      HttpMethod::Put => axum::http::Method::PUT.as_str(),
      HttpMethod::Delete => axum::http::Method::DELETE.as_str(),
      HttpMethod::Head => axum::http::Method::HEAD.as_str(),
      HttpMethod::Trace => axum::http::Method::TRACE.as_str(),
      HttpMethod::Connect => axum::http::Method::CONNECT.as_str(),
      HttpMethod::Patch => axum::http::Method::PATCH.as_str(),
      _ => "Unknown",
    };
  }
}

impl Default for HttpMethod {
  fn default() -> Self {
    return Self::Unknown;
  }
}

impl std::fmt::Display for HttpMethod {
  fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
    return fmt.write_str(self.as_str());
  }
}

impl From<&axum::http::Method> for HttpMethod {
  fn from(value: &axum::http::Method) -> Self {
    return match *value {
      axum::http::Method::OPTIONS => HttpMethod::Options,
      axum::http::Method::GET => HttpMethod::Get,
      axum::http::Method::POST => HttpMethod::Post,
      axum::http::Method::PUT => HttpMethod::Put,
      axum::http::Method::DELETE => HttpMethod::Delete,
      axum::http::Method::HEAD => HttpMethod::Head,
      axum::http::Method::TRACE => HttpMethod::Trace,
      axum::http::Method::CONNECT => HttpMethod::Connect,
      axum::http::Method::PATCH => HttpMethod::Patch,
      _ => HttpMethod::Unknown,
    };
  }
}

impl From<&str> for HttpMethod {
  fn from(value: &str) -> Self {
    return match value {
      "OPTIONS" => HttpMethod::Options,
      "GET" => HttpMethod::Get,
      "POST" => HttpMethod::Post,
      "PUT" => HttpMethod::Put,
      "DELETE" => HttpMethod::Delete,
      "HEAD" => HttpMethod::Head,
      "TRACE" => HttpMethod::Trace,
      "CONNECT" => HttpMethod::Connect,
      "PATCH" => HttpMethod::Patch,
      _ => HttpMethod::Unknown,
    };
  }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
enum HttpVersion {
  Unknown,
  Http09,
  Http10,
  Http11,
  Http20,
  Http30,
}

impl Default for HttpVersion {
  fn default() -> Self {
    return Self::Unknown;
  }
}

impl HttpVersion {
  fn as_str(&self) -> &'static str {
    return match self {
      Self::Http09 => "HTTP/0.9",
      Self::Http10 => "HTTP/1.0",
      Self::Http11 => "HTTP/1.1",
      Self::Http20 => "HTTP/2.0",
      Self::Http30 => "HTTP/3.0",
      Self::Unknown => "HTTP/?",
    };
  }
}

impl std::fmt::Display for HttpVersion {
  fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
    return fmt.write_str(self.as_str());
  }
}

impl From<&str> for HttpVersion {
  fn from(value: &str) -> Self {
    return match value {
      "HTTP/0.9" => Self::Http09,
      "HTTP/1.0" => Self::Http10,
      "HTTP/1.1" => Self::Http11,
      "HTTP/2.0" => Self::Http20,
      "HTTP/3.0" => Self::Http30,
      _ => HttpVersion::Unknown,
    };
  }
}

// NOTE: Tracing is quite sweet but also utterly decoupled. There are several moving parts.
//
//  * In `server/mod.rs` we install some tower/axum middleware: `tower_http::trace::TraceLayer` to
//    install hooks for managing tracing spans, events, ... .
//  * These hooks (in this file) are where we define *what* gets put into a trace span and what
//    events are emitted.
//  * Independently, we install the `SqliteLogLayer` as a tracing subscriber listening for above
//    events, building request-response log entries and ultimately sending them to a writer task.
//  * The writer task receives the request-response log entries writes them to the logs database.
//  * Lastly, there's also a period task to wipe expired logs past their retention.
#[repr(i64)]
#[derive(Debug, Clone, Deserialize, Serialize)]
enum LogType {
  Undefined = 0,
  AdminRequest = 1,
  HttpRequest = 2,
  RecordApiRequest = 3,
}

const SPAN_NAME: &str = "http_span";
const EVENT_NAME: &str = "http_event";
pub(crate) const EVENT_TARGET: &str = "http_target";
pub(crate) const LEVEL: Level = Level::INFO;

pub(super) fn sqlite_logger_make_span(request: &Request<Body>) -> Span {
  let headers = request.headers();

  let client_ip = InsecureClientIp::from(headers, request.extensions())
    .map(|ip| ip.0.to_string())
    .ok();

  // NOTE: "%" means print using fmt::Display, and "?" means fmt::Debug.
  return tracing::span!(
      target: EVENT_TARGET,
      LEVEL,
      SPAN_NAME,
      method = %request.method(),
      uri = %request.uri(),
      version = ?request.version(),
      host = get_header(headers, "host"),
      client_ip,
      user_agent = get_header(headers, "user-agent"),
      referer = get_header(headers, "referer"),
      // Reserve placeholders that may be recorded later.
      user_id = tracing::field::Empty,
      latency_ms = tracing::field::Empty,
      status = tracing::field::Empty,
      length = tracing::field::Empty,
  );
}

pub(super) fn sqlite_logger_on_request(_req: &Request<Body>, _span: &Span) {
  // We don't need to record anything extra, since we already unpacked the request during span
  // creation above.
}

pub(super) fn sqlite_logger_on_response(response: &Response<Body>, latency: Duration, span: &Span) {
  span.record("latency_ms", as_millis_f64(&latency));
  span.record("status", response.status().as_u16());

  // NOTE: The `tower::Limited` body may provide for a more robust implementation when the header
  // is not set. If the tower layer runs beforre this event, we could probably just have our own
  // `RequestBodyLimitLayer` implementation logging the content length there.
  if let Some(header) = get_header(response.headers(), header::CONTENT_LENGTH) {
    span.record("length", header.parse::<i64>().ok());
  }

  // NOTE: We could hand-craft an event w/o using the macro, we could attach less unused metadata
  // like module, code line, ... . Same goes for span!() above.
  //
  // let metadata = span.metadata().expect("metadata");
  // tracing::Event::child_of(
  //   span.id(),
  //   metadata,
  //   // &tracing::valueset! { metadata.fields(), EVENT_TARGET, EVENT_NAME, file, line,
  // module_path, ?params },   &tracing::valueset! { metadata.fields(), EVENT_TARGET, EVENT_NAME
  // }, );

  // Log the event that gets picked up by `SqilteLogLayer` and written out.
  tracing::event!(
    name: EVENT_NAME,
    target: EVENT_TARGET,
    parent: span,
    LEVEL,
    {}
  );
}

pub struct SqliteLogLayer {
  sender: tokio::sync::mpsc::UnboundedSender<Box<LogFieldStorage>>,

  #[allow(unused)]
  json_stdout: bool,
}

impl SqliteLogLayer {
  pub fn new(state: &AppState, json_stdout: bool) -> Self {
    // NOTE: We're boxing the channel contents to lower the growth rate of back-stopped unbound
    // channels. The underlying container doesn't seem to every shrink :/.
    //
    // TODO: should we use a bounded receiver to create back-pressure?
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();

    let conn = state.logs_conn().clone();
    let rt = tokio::runtime::Handle::current();
    rt.spawn(async move {
      const LIMIT: usize = 128;
      let mut buffer = Vec::<Box<LogFieldStorage>>::with_capacity(LIMIT);

      while receiver.recv_many(&mut buffer, LIMIT).await > 0 {
        let logs = std::mem::take(&mut buffer);

        let result = conn
          .call(move |conn| {
            if logs.len() > 1 {
              let tx = conn.transaction()?;
              for log in logs {
                Self::insert_log(&tx, log)?;
              }
              tx.commit()?;
            } else {
              for log in logs {
                Self::insert_log(conn, log)?
              }
            }

            Ok(())
          })
          .await;

        if let Err(err) = result {
          log::warn!("Failed to send logs: {err}");
        }
      }
    });

    return SqliteLogLayer {
      sender,
      json_stdout,
    };
  }

  // The writer runs in a separate Task in the background and receives Logs via a channel, which it
  // then writes to Sqlite.
  #[inline]
  fn write_log(&self, storage: LogFieldStorage) {
    if self.json_stdout {
      let json: JsonLog = (&storage).into();
      tokio::spawn(async move {
        let mut buf: Vec<u8> = Vec::with_capacity(480);
        if serde_json::to_writer(&mut buf, &json).is_ok() {
          buf.push(b'\n');
          let _ = tokio::io::stdout().write_all(&buf).await;
        }
      });
    }

    if let Err(err) = self.sender.send(Box::new(storage)) {
      panic!("Sending logs failed: {err}");
    }
  }

  #[inline]
  fn insert_log(
    conn: &rusqlite::Connection,
    storage: Box<LogFieldStorage>,
  ) -> Result<(), rusqlite::Error> {
    #[cfg(test)]
    if !storage.fields.is_empty() {
      log::warn!("Dangling fields: {:?}", storage.fields);
    }

    lazy_static::lazy_static! {
      static ref QUERY: String = indoc::formatdoc! {"
        INSERT INTO
          _logs (created, status, method, url, latency, client_ip, referer, user_agent, user_id)
        VALUES
          ($1, $2, $3, $4, $5, $6, $7, $8, $9)
      "};
    }

    let mut stmt = conn.prepare_cached(&QUERY)?;
    stmt.execute((
      as_seconds_f64(
        storage
          .timestamp
          .signed_duration_since(chrono::DateTime::UNIX_EPOCH),
      ),
      storage.status,
      storage.method.as_str(),
      storage.uri,
      storage.latency_ms,
      // client_ip is defined as NOT NULL in the schema :/.
      storage.client_ip.unwrap_or_default(),
      storage.referer,
      storage.user_agent,
      if storage.user_id > 0 {
        rusqlite::types::Value::Blob(Uuid::from_u128(storage.user_id).into_bytes().to_vec())
      } else {
        rusqlite::types::Value::Null
      },
      // TODO: we're yet not writing extra JSON data to the data field.
    ))?;

    return Ok(());
  }
}

impl<S> Layer<S> for SqliteLogLayer
where
  S: tracing::Subscriber,
  S: for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
  /// When a new "__tbreq" span is created, attach field storage.
  fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
    let span = ctx.span(id).expect("span must exist in on_new_span");

    let mut storage = LogFieldStorage::default();
    attrs.record(&mut LogVisitor(&mut storage));
    span.extensions_mut().insert(storage);
  }

  // Add events to field storage.
  fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
    // Is it a response log event?
    if event.metadata().target() != EVENT_TARGET {
      return;
    }
    assert_eq!(event.metadata().name(), EVENT_NAME);

    let Some(span) = ctx.event_span(event) else {
      log::warn!("orphaned '{EVENT_TARGET}' event");
      return;
    };
    assert_eq!(span.name(), SPAN_NAME);

    let mut extensions = span.extensions_mut();
    let Some(mut storage) = extensions.remove::<LogFieldStorage>() else {
      log::warn!("LogFieldStorage ext missing. Already consumed?");
      return;
    };

    storage.timestamp = chrono::Utc::now();

    // Collect the remaining data from the event itself.
    event.record(&mut LogVisitor(&mut storage));

    // Then write.
    self.write_log(storage);
  }

  // When span.record() is called, add to field storage.
  fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
    let Some(span) = ctx.span(id) else {
      return;
    };

    if !values.is_empty() {
      let mut extensions = span.extensions_mut();
      if let Some(storage) = extensions.get_mut::<LogFieldStorage>() {
        values.record(&mut LogVisitor(storage));
      }
    }
  }
}

/// HTTP Request/Response properties to be logged.
#[derive(Debug, Default, Clone)]
struct LogFieldStorage {
  timestamp: chrono::DateTime<chrono::Utc>,
  method: HttpMethod,
  uri: String,
  client_ip: Option<String>,
  host: String,
  referer: String,
  user_agent: String,
  user_id: u128,
  version: HttpVersion,

  // Response fields/properties
  status: u64,
  latency_ms: f64,
  length: i64,

  // All other fields.
  fields: serde_json::Map<String, serde_json::Value>,
}

/// Defines the JSON output format for stdout logging.
#[derive(Debug, Default, Clone, Serialize)]
struct JsonLog {
  /// Response timestamp in seconds since epoch with fractional milliseconds.
  timestamp: String,
  /// HTTP method (e.g. GET, POST).
  method: HttpMethod,
  /// HTTP version.
  version: HttpVersion,
  /// Host part.
  host: String,
  /// Request URI.
  uri: String,
  /// HTTP Referer
  referer: String,
  /// HTTP User agent.
  user_agent: String,
  /// User id.
  user: u128,
  /// Client ip address.
  client_ip: Option<String>,

  // HTTP response status code.
  status: u64,
  // latency in milliseconds with fractional microseconds.
  latency_ms: f64,
}

impl From<&LogFieldStorage> for JsonLog {
  fn from(storage: &LogFieldStorage) -> Self {
    return Self {
      timestamp: storage.timestamp.to_rfc3339(),
      method: storage.method.clone(),
      version: storage.version.clone(),
      host: storage.host.clone(),
      uri: storage.uri.clone(),
      referer: storage.referer.clone(),
      user_agent: storage.user_agent.clone(),
      user: storage.user_id,
      client_ip: storage.client_ip.clone(),
      status: storage.status,
      latency_ms: storage.latency_ms,
    };
  }
}

struct LogVisitor<'a>(&'a mut LogFieldStorage);

impl tracing::field::Visit for LogVisitor<'_> {
  fn record_f64(&mut self, field: &Field, double: f64) {
    match field.name() {
      "latency_ms" => self.0.latency_ms = double,
      name => {
        self.0.fields.insert(name.into(), double.into());
      }
    };
  }

  fn record_i64(&mut self, field: &Field, int: i64) {
    match field.name() {
      "length" => self.0.length = int,
      name => {
        self.0.fields.insert(name.into(), int.into());
      }
    };
  }

  fn record_u64(&mut self, field: &Field, uint: u64) {
    match field.name() {
      "status" => self.0.status = uint,
      name => {
        self.0.fields.insert(name.into(), uint.into());
      }
    };
  }

  fn record_u128(&mut self, field: &Field, int: u128) {
    match field.name() {
      "user_id" => self.0.user_id = int,
      name => {
        self.0.fields.insert(name.into(), int.to_string().into());
      }
    };
  }

  fn record_bool(&mut self, field: &Field, b: bool) {
    self.0.fields.insert(field.name().into(), b.into());
  }

  fn record_str(&mut self, field: &Field, s: &str) {
    match field.name() {
      "client_ip" => self.0.client_ip = Some(s.to_string()),
      "host" => self.0.host = s.to_string(),
      "referer" => self.0.referer = s.to_string(),
      "user_agent" => self.0.user_agent = s.to_string(),
      name => {
        self.0.fields.insert(name.into(), s.into());
      }
    };
  }

  fn record_debug(&mut self, field: &Field, dbg: &dyn std::fmt::Debug) {
    match field.name() {
      "method" => self.0.method = format!("{:?}", dbg).as_str().into(),
      "uri" => self.0.uri = format!("{:?}", dbg),
      "version" => self.0.version = format!("{:?}", dbg).as_str().into(),
      _name => {
        // Skip "messages" and other fields, we only log structured data.
      }
    };
  }

  fn record_error(&mut self, field: &Field, err: &(dyn std::error::Error + 'static)) {
    self
      .0
      .fields
      .insert(field.name().into(), json!(err.to_string()));
  }
}

#[inline]
fn as_millis_f64(d: &Duration) -> f64 {
  const NANOS_PER_MILLI: f64 = 1_000_000.0;
  const MILLIS_PER_SEC: u64 = 1_000;

  return (d.as_secs() as f64) * (MILLIS_PER_SEC as f64)
    + (d.subsec_nanos() as f64) / (NANOS_PER_MILLI);
}

#[inline]
fn as_seconds_f64(d: chrono::Duration) -> f64 {
  const NANOS_PER_SECOND: f64 = 1_000_000_000.0;
  return (d.num_seconds() as f64) + (d.subsec_nanos() as f64) / (NANOS_PER_SECOND);
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  #[test]
  fn test_as_seconds_f64() {
    let duration = chrono::Duration::new(1, 250_000_000).unwrap();
    assert_eq!(1.25, as_seconds_f64(duration));
  }

  #[test]
  fn test_as_millis_f64() {
    let duration = Duration::new(1, 1_000_000_000);
    assert_eq!(2000.0, as_millis_f64(&duration));

    let duration = Duration::new(1, 250_000_000);
    assert_eq!(1250.0, as_millis_f64(&duration));
  }
}
