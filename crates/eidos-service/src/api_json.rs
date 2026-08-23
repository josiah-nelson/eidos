//! JSON response adapter for JavaScript-safe 64-bit integers.
//!
//! JavaScript numbers cannot preserve every `i64`/`u64`. HTTP responses use
//! decimal strings for those Rust types, while 32-bit integers and floats
//! remain JSON numbers. This policy applies recursively, including values
//! owned by the domain and catalog crates.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::ser::Formatter;
use std::io::{self, Write};

pub struct ApiJson<T>(pub T);

#[derive(Default)]
struct StringifyLargeIntegers;

fn write_decimal_string<W: ?Sized + Write, T: std::fmt::Display>(
    writer: &mut W,
    value: T,
) -> io::Result<()> {
    write!(writer, "\"{value}\"")
}

impl Formatter for StringifyLargeIntegers {
    fn write_i64<W: ?Sized + Write>(&mut self, writer: &mut W, value: i64) -> io::Result<()> {
        write_decimal_string(writer, value)
    }

    fn write_u64<W: ?Sized + Write>(&mut self, writer: &mut W, value: u64) -> io::Result<()> {
        write_decimal_string(writer, value)
    }
}

impl<T: Serialize> IntoResponse for ApiJson<T> {
    fn into_response(self) -> Response {
        let mut bytes = Vec::new();
        let mut serializer =
            serde_json::Serializer::with_formatter(&mut bytes, StringifyLargeIntegers);
        match self.0.serialize(&mut serializer) {
            Ok(()) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize API response: {error}"),
            )
                .into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::json;

    #[tokio::test]
    async fn stringifies_only_large_rust_integer_types() {
        #[derive(Serialize)]
        struct Shape {
            signed: i64,
            unsigned: u64,
            count: u32,
            ratio: f64,
            nested: serde_json::Value,
        }

        let response = ApiJson(Shape {
            signed: i64::MIN,
            unsigned: u64::MAX,
            count: 7,
            ratio: 1.5,
            nested: json!({ "value": 9_007_199_254_740_993u64 }),
        })
        .into_response();
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            json!({
                "signed": i64::MIN.to_string(),
                "unsigned": u64::MAX.to_string(),
                "count": 7,
                "ratio": 1.5,
                "nested": { "value": "9007199254740993" }
            })
        );
    }
}
