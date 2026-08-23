//! Stable JSON encodings for integers JavaScript cannot represent exactly.

use serde::de::Error;
use serde::{Deserialize, Deserializer, Serializer};
use std::fmt::Display;
use std::str::FromStr;

#[derive(Deserialize)]
#[serde(untagged)]
enum Decimal<T> {
    String(String),
    Number(T),
}

impl<T> Decimal<T>
where
    T: FromStr,
    T::Err: Display,
{
    fn value<E: Error>(self) -> Result<T, E> {
        match self {
            Self::String(value) => value.parse().map_err(Error::custom),
            Self::Number(value) => Ok(value),
        }
    }
}

pub mod i64_string {
    use super::*;

    pub fn serialize<S: Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
        Decimal::<i64>::deserialize(deserializer)?.value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Values {
        #[serde(with = "i64_string")]
        signed: i64,
    }

    #[test]
    fn decimal_strings_round_trip_and_legacy_numbers_still_parse() {
        let values = Values { signed: i64::MIN };
        assert_eq!(
            serde_json::to_string(&values).unwrap(),
            r#"{"signed":"-9223372036854775808"}"#
        );
        assert_eq!(
            serde_json::from_str::<Values>(r#"{"signed":-7}"#).unwrap(),
            Values { signed: -7 }
        );
    }
}
