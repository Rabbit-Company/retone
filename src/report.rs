use serde_json::{Map, Value};
use std::fmt::{self, Display};
use std::io::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Progress {
  /// Progress and results as text on standard error
  Text,
  /// One JSON object per line on standard output
  Json,
  /// Nothing except errors
  None,
}

#[derive(Debug)]
pub struct Failure {
  pub code: &'static str,
  pub message: String,
}

impl Display for Failure {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.message)
  }
}

impl std::error::Error for Failure {}

pub fn fail(code: &'static str, message: impl Display) -> anyhow::Error {
  anyhow::Error::new(Failure {
    code,
    message: message.to_string(),
  })
}

pub trait Coded<T> {
  fn code(self, code: &'static str) -> anyhow::Result<T>;
}

// Errors from rabbit-av1 carry a code of their own, as a Failure does.
fn cause_code(cause: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
  if let Some(failure) = cause.downcast_ref::<Failure>() {
    return Some(failure.code);
  }
  cause.downcast_ref::<rabbit_av1::Error>().map(rabbit_av1::Error::code)
}

impl<T, E: Into<anyhow::Error>> Coded<T> for Result<T, E> {
  fn code(self, code: &'static str) -> anyhow::Result<T> {
    self.map_err(|error| {
      let error = error.into();
      if error.chain().any(|cause| cause_code(cause).is_some()) {
        error
      } else {
        fail(code, format!("{error:#}"))
      }
    })
  }
}

pub fn error_code(error: &anyhow::Error) -> &'static str {
  error.chain().find_map(cause_code).unwrap_or("error")
}

// Writes one JSON object to standard output, with the event name first.
pub fn event(name: &str, fields: Vec<(&str, Value)>) {
  let mut object = Map::new();
  object.insert("event".to_string(), Value::from(name));
  for (key, value) in fields {
    object.insert(key.to_string(), value);
  }
  let mut stdout = std::io::stdout().lock();
  let _ = writeln!(stdout, "{}", Value::Object(object));
  let _ = stdout.flush();
}

// Rounds to three decimals, so JSON output does not carry float noise.
pub fn number(value: f64) -> Value {
  if value.is_finite() {
    Value::from((value * 1000.0).round() / 1000.0)
  } else {
    Value::Null
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn keeps_the_innermost_code() {
    let inner: anyhow::Result<()> = Err(fail("frame_count_mismatch", "the video has 10 frames"));
    let wrapped = inner.code("io_error").map_err(|error| error.context("while applying"));
    assert_eq!(error_code(&wrapped.unwrap_err()), "frame_count_mismatch");
    let plain: Result<(), std::io::Error> = Err(std::io::Error::other("disk full"));
    let error = plain.code("io_error").unwrap_err();
    assert_eq!(error_code(&error), "io_error");
    assert_eq!(error.to_string(), "disk full");
    assert_eq!(error_code(&anyhow::anyhow!("other")), "error");
  }

  #[test]
  fn keeps_rabbit_av1_codes() {
    let inner: Result<(), rabbit_av1::Error> = Err(rabbit_av1::Error::new(rabbit_av1::ErrorKind::InvalidBitstream, "truncated"));
    let error = inner.code("invalid_metadata").unwrap_err();
    assert_eq!(error_code(&error), "invalid_bitstream");
  }

  #[test]
  fn rounds_numbers() {
    assert_eq!(number(1.23456).to_string(), "1.235");
    assert_eq!(number(f64::NAN), Value::Null);
  }
}
