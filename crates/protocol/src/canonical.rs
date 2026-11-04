//! Strict tool-argument JSON for the versioned output projection. No duplicate keys or unsafe numbers.
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use std::fmt;
const SAFE_NUMBER: f64 = 9_007_199_254_740_991.0;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalError {
    InvalidJson,
    IncompleteJson,
    InvalidArguments,
    TooLarge,
}
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(StrictVisitor)
    }
}
struct StrictVisitor;
impl<'de> Visitor<'de> for StrictVisitor {
    type Value = StrictValue;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded, unambiguous interoperable JSON")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(v)))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(v.into())))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(v)))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        if v.unsigned_abs() > 9_007_199_254_740_991 {
            return Err(E::custom("large exact numbers must be strings"));
        }
        Ok(StrictValue(Value::Number(v.into())))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        if v > 9_007_199_254_740_991 {
            return Err(E::custom("large exact numbers must be strings"));
        }
        Ok(StrictValue(Value::Number(v.into())))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        if !v.is_finite() || v.abs() > SAFE_NUMBER {
            return Err(E::custom("number outside projection profile bounds"));
        }
        Ok(StrictValue(Value::Number(
            Number::from_f64(v).ok_or_else(|| E::custom("invalid number"))?,
        )))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(StrictValue(value)) = seq.next_element()? {
            values.push(value);
        }
        Ok(StrictValue(Value::Array(values)))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate JSON key"));
            }
            let StrictValue(value) = map.next_value()?;
            values.insert(key, value);
        }
        Ok(StrictValue(Value::Object(values)))
    }
}
pub fn parse_tool_arguments(input: &str) -> Result<Value, CanonicalError> {
    if input.len() > 1024 * 1024 {
        return Err(CanonicalError::TooLarge);
    }
    let StrictValue(value) = serde_json::from_str(input).map_err(|error: serde_json::Error| {
        if error.is_eof() {
            CanonicalError::IncompleteJson
        } else if error.is_data() {
            CanonicalError::InvalidArguments
        } else {
            CanonicalError::InvalidJson
        }
    })?;
    if !value.is_object() {
        return Err(CanonicalError::InvalidArguments);
    }
    Ok(value)
}
pub fn canonical_tool_arguments(input: &str) -> Result<Vec<u8>, CanonicalError> {
    serde_jcs::to_vec(&parse_tool_arguments(input)?).map_err(|_| CanonicalError::InvalidArguments)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uses_jcs_utf16_order_and_ecmascript_numbers_without_unicode_normalization() {
        let result =
            canonical_tool_arguments("{\"￿\":1,\"😀\":2,\"n\":1.0,\"zero\":-0.0,\"accent\":\"é\"}")
                .expect("JCS");
        assert_eq!(
            String::from_utf8(result).expect("UTF8"),
            "{\"accent\":\"é\",\"n\":1,\"zero\":0,\"😀\":2,\"￿\":1}"
        );
    }
    #[test]
    fn rejects_duplicate_keys_partial_arguments_and_precision_ambiguity() {
        for raw in [
            "{\"a\":1,\"a\":2}",
            "{\"x\":{\"a\":1,\"a\":2}}",
            "{\"x\":",
            "{\"x\":9007199254740992}",
            "{\"x\":1e30}",
            "{\"x\":1e999}",
            "[]",
            "{\"x\":\"\\ud800\"}",
        ] {
            assert!(
                parse_tool_arguments(raw).is_err(),
                "accepted invalid fixture"
            );
        }
        assert!(parse_tool_arguments("{\"id\":\"9223372036854775807\"}").is_ok());
    }
}
