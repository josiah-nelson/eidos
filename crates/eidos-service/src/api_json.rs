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
struct StringifyLargeIntegers {
    in_string: bool,
}

fn write_decimal_string<W: ?Sized + Write, T: std::fmt::Display>(
    writer: &mut W,
    value: T,
) -> io::Result<()> {
    write!(writer, "\"{value}\"")
}

impl Formatter for StringifyLargeIntegers {
    fn begin_string<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.in_string = true;
        writer.write_all(b"\"")
    }

    fn end_string<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.in_string = false;
        writer.write_all(b"\"")
    }

    fn write_i64<W: ?Sized + Write>(&mut self, writer: &mut W, value: i64) -> io::Result<()> {
        // serde_json already surrounds numeric object keys with begin/end_string.
        // Adding another pair of quotes produces invalid JSON for populated maps.
        if self.in_string {
            write!(writer, "{value}")
        } else {
            write_decimal_string(writer, value)
        }
    }

    fn write_u64<W: ?Sized + Write>(&mut self, writer: &mut W, value: u64) -> io::Result<()> {
        if self.in_string {
            write!(writer, "{value}")
        } else {
            write_decimal_string(writer, value)
        }
    }
}

/// Serialize any API-owned JSON using the v2 exact-integer policy. Streaming
/// responses use this too, so they cannot quietly diverge from `ApiJson`.
pub(crate) fn to_vec<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut bytes, StringifyLargeIntegers::default());
    value.serialize(&mut serializer)?;
    Ok(bytes)
}

impl<T: Serialize> IntoResponse for ApiJson<T> {
    fn into_response(self) -> Response {
        match to_vec(&self.0) {
            Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
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

    #[test]
    fn numeric_map_keys_are_quoted_once_and_values_remain_exact() {
        use std::collections::BTreeMap;
        #[derive(Serialize)]
        struct Shape {
            signed: BTreeMap<i64, u64>,
            unsigned: BTreeMap<u64, i64>,
            strings: BTreeMap<String, u64>,
            trailing: i64,
        }
        let bytes = to_vec(&Shape {
            signed: BTreeMap::from([(i64::MIN, u64::MAX)]),
            unsigned: BTreeMap::from([(u64::MAX, i64::MIN)]),
            strings: BTreeMap::from([("quoted\"key".into(), u64::MAX)]),
            trailing: 9_007_199_254_740_993,
        })
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["signed"][i64::MIN.to_string()], u64::MAX.to_string());
        assert_eq!(
            parsed["unsigned"][u64::MAX.to_string()],
            i64::MIN.to_string()
        );
        assert_eq!(parsed["strings"]["quoted\"key"], u64::MAX.to_string());
        assert_eq!(parsed["trailing"], "9007199254740993");
    }

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
