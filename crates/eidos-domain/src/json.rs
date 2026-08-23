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

pub mod u64_string {
    use super::*;

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        Decimal::<u64>::deserialize(deserializer)?.value()
    }
}

pub mod option_u64_string {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => serializer.serialize_some(&value.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        Option::<Decimal<u64>>::deserialize(deserializer)?
            .map(Decimal::value)
            .transpose()
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
        #[serde(with = "u64_string")]
        unsigned: u64,
        #[serde(default, with = "option_u64_string")]
        optional: Option<u64>,
    }

    #[test]
    fn decimal_strings_round_trip_and_legacy_numbers_still_parse() {
        let values = Values {
            signed: i64::MIN,
            unsigned: u64::MAX,
            optional: Some(9_007_199_254_740_993),
        };
        assert_eq!(
            serde_json::to_string(&values).unwrap(),
            r#"{"signed":"-9223372036854775808","unsigned":"18446744073709551615","optional":"9007199254740993"}"#
        );
        assert_eq!(
            serde_json::from_str::<Values>(r#"{"signed":-7,"unsigned":8}"#).unwrap(),
            Values {
                signed: -7,
                unsigned: 8,
                optional: None,
            }
        );
    }
}
