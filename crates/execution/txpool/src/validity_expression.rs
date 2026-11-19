//! Wire representation of validity conditions and their transaction-wide decode limit.

use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{
        self, DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor,
        value::MapAccessDeserializer,
    },
};

use crate::{DEFAULT_MAX_VALIDITY_PREDICATES, ValidityPredicate, ValidityPredicateError};

/// Normal form used for signing, forwarding and evaluation. At ingress, `allOf` and `anyOf`
/// may nest; the decoder distributes AND over OR without exceeding the wire ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ValidityExpression {
    /// Any complete group may satisfy the expression.
    #[serde(rename = "anyOf")]
    AnyOf(Vec<Vec<ValidityPredicate>>),
    /// Every predicate must hold. Serialized as the original flat array.
    #[serde(untagged)]
    All(Vec<ValidityPredicate>),
}

impl Default for ValidityExpression {
    fn default() -> Self {
        Self::All(Vec::new())
    }
}

impl From<Vec<ValidityPredicate>> for ValidityExpression {
    fn from(value: Vec<ValidityPredicate>) -> Self {
        Self::All(value)
    }
}

impl ValidityExpression {
    /// Conjunctions in submission order, including the sole legacy conjunction.
    pub fn groups(&self) -> &[Vec<ValidityPredicate>] {
        match self {
            Self::All(group) => std::slice::from_ref(group),
            Self::AnyOf(groups) => groups,
        }
    }

    /// Total number of predicates across alternatives.
    pub fn len(&self) -> usize {
        self.groups().iter().map(Vec::len).sum()
    }

    /// Distributes a conjunction over alternatives, checking the number of output leaves
    /// *before* allocating the Cartesian product. Both operands are bounded decoder outputs.
    fn and(mut self, rhs: Self, limit: usize) -> Result<Self, ValidityPredicateError> {
        if let Self::All(left) = &mut self
            && let Self::All(right) = rhs
        {
            // The parser already restricted rhs to the remaining leaf budget.
            left.extend(right);
            return Ok(self);
        }
        let count = self.len() * rhs.groups().len() + rhs.len() * self.groups().len();
        if count > limit {
            return Err(ValidityPredicateError::TooMany { count, max: limit });
        }
        let groups = self
            .groups()
            .iter()
            .flat_map(|left| {
                rhs.groups().iter().map(move |right| {
                    let mut combined = Vec::with_capacity(left.len() + right.len());
                    combined.extend_from_slice(left);
                    combined.extend_from_slice(right);
                    combined
                })
            })
            .collect();
        Ok(Self::AnyOf(groups))
    }

    /// Only an empty legacy list denotes the absence of a sidecar.
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::All(group) if group.is_empty())
    }

    /// Validates every branch, applying the configured count limit to the whole expression.
    pub fn validate(&self, max: usize) -> Result<(), ValidityPredicateError> {
        let count = self.len();
        if count > max {
            return Err(ValidityPredicateError::TooMany { count, max });
        }
        if self.groups().is_empty() {
            return Err(ValidityPredicateError::Empty);
        }
        for group in self.groups() {
            ValidityPredicate::validate_batch(group, max)?;
        }
        Ok(())
    }
}

struct PredicateList(usize);
impl<'de> DeserializeSeed<'de> for PredicateList {
    type Value = Vec<ValidityPredicate>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}
impl<'de> Visitor<'de> for PredicateList {
    type Value = Vec<ValidityPredicate>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list of validity predicates")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut predicates = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(self.0));
        while let Some(p) = seq.next_element()? {
            if predicates.len() == self.0 {
                return Err(de::Error::custom(format!(
                    "too many validity predicates: maximum {DEFAULT_MAX_VALIDITY_PREDICATES}"
                )));
            }
            predicates.push(p);
        }
        Ok(predicates)
    }
}

/// Maximum number of logical operators on a path from the root to a predicate.
const MAX_EXPRESSION_DEPTH: usize = 16;

#[derive(Clone, Copy)]
struct ExpressionDecoder {
    budget: usize,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for ExpressionDecoder {
    type Value = ValidityExpression;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        if self.budget == 0 {
            return Err(de::Error::custom(format!(
                "too many validity predicates: maximum {DEFAULT_MAX_VALIDITY_PREDICATES}"
            )));
        }
        d.deserialize_any(self)
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "camelCase")]
enum Field {
    AllOf,
    AnyOf,
    Type,
    Params,
}

impl<'de> Visitor<'de> for ExpressionDecoder {
    type Value = ValidityExpression;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a validity predicate, predicate list, allOf or anyOf")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        let predicates = PredicateList(self.budget).visit_seq(seq)?;
        if self.depth != 0 && predicates.is_empty() {
            return Err(de::Error::custom("empty validity group"));
        }
        Ok(ValidityExpression::All(predicates))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let field =
            map.next_key::<Field>()?.ok_or_else(|| de::Error::custom("empty validity object"))?;
        let any = match field {
            Field::AnyOf => true,
            Field::AllOf => false,
            Field::Type | Field::Params => {
                let first = if matches!(field, Field::Type) { "type" } else { "params" };
                let leaf =
                    ValidityPredicate::deserialize(MapAccessDeserializer::new(PredicateMap {
                        first: Some(first),
                        map,
                    }))?;
                return Ok(ValidityExpression::All(vec![leaf]));
            }
        };
        if self.depth >= MAX_EXPRESSION_DEPTH {
            return Err(de::Error::custom(format!(
                "validity expression exceeds {MAX_EXPRESSION_DEPTH} levels"
            )));
        }
        let result =
            map.next_value_seed(Operands { decoder: Self { depth: self.depth + 1, ..self }, any })?;
        if map.next_key::<de::IgnoredAny>()?.is_some() {
            return Err(de::Error::custom("validity operator must be the only field"));
        }
        Ok(result)
    }
}

/// Replay the discriminating key so leaf validation (including params-first JSON, duplicate
/// fields and default masks) stays entirely with `ValidityPredicate`, without buffering a tree.
struct PredicateMap<A> {
    first: Option<&'static str>,
    map: A,
}
impl<'de, A: MapAccess<'de>> MapAccess<'de> for PredicateMap<A> {
    type Error = A::Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        if let Some(key) = self.first.take() {
            seed.deserialize(key.into_deserializer()).map(Some)
        } else {
            self.map.next_key_seed(seed)
        }
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        self.map.next_value_seed(seed)
    }
}

struct Operands {
    decoder: ExpressionDecoder,
    any: bool,
}
impl<'de> DeserializeSeed<'de> for Operands {
    type Value = ValidityExpression;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_seq(self)
    }
}
impl<'de> Visitor<'de> for Operands {
    type Value = ValidityExpression;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a non-empty array of validity operands")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        if self.any {
            let mut groups = Vec::new();
            let mut decoder = self.decoder;
            while let Some(term) = seq.next_element_seed(decoder)? {
                decoder.budget -= term.len();
                match term {
                    ValidityExpression::All(group) => groups.push(group),
                    ValidityExpression::AnyOf(children) => groups.extend(children),
                }
            }
            if groups.is_empty() {
                return Err(de::Error::custom("empty anyOf"));
            }
            return Ok(ValidityExpression::AnyOf(groups));
        }
        let mut result =
            seq.next_element_seed(self.decoder)?.ok_or_else(|| de::Error::custom("empty allOf"))?;
        while let Some(next) = seq.next_element_seed(ExpressionDecoder {
            budget: self.decoder.budget - result.len(),
            ..self.decoder
        })? {
            result = result.and(next, self.decoder.budget).map_err(de::Error::custom)?;
        }
        Ok(result)
    }
}

/// Reads the bounded legacy flat representation without buffering arbitrary JSON.
pub fn deserialize_bounded_predicates<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Vec<ValidityPredicate>, D::Error> {
    PredicateList(DEFAULT_MAX_VALIDITY_PREDICATES).deserialize(d)
}

impl<'de> Deserialize<'de> for ValidityExpression {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        ExpressionDecoder { budget: DEFAULT_MAX_VALIDITY_PREDICATES, depth: 0 }.deserialize(d)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rejects_oversized_products_before_materializing_them() {
        let leaf = json!({"type":"block_number", "params":{"op":"=","value":"0x1"}});
        let choice = json!({"anyOf":[leaf, leaf]});
        // Four binary choices produce 16 groups of four.
        let full = json!({"allOf":vec![choice.clone();4]});
        let decoded: ValidityExpression = serde_json::from_value(full).unwrap();
        assert_eq!(decoded.len(), 64);
        assert!(decoded.validate(63).is_err());
        // Just twelve submitted operands, but 36 pairs (72 leaves) after distribution.
        let six = json!({"anyOf":vec![leaf;6]});
        let overflow = json!({"allOf":[six,six]});
        assert!(
            serde_json::from_value::<ValidityExpression>(overflow)
                .unwrap_err()
                .to_string()
                .contains("72")
        );
    }

    #[test]
    fn bounds_nesting_even_for_single_child_operators() {
        let mut expr = json!({"type":"block_number","params":{"op":"<","value":"0x4"}});
        for _ in 0..MAX_EXPRESSION_DEPTH {
            expr = json!({"allOf":[expr]});
        }
        assert!(serde_json::from_value::<ValidityExpression>(expr.clone()).is_ok());
        assert!(serde_json::from_value::<ValidityExpression>(json!({"anyOf":[expr]})).is_err());
    }

    #[test]
    fn nested_leaves_keep_the_existing_strict_predicate_schema() {
        for leaf in [
            r#"{"params":{"op":"=","value":"0x4"},"type":"block_number"}"#,
            r#"{"type":"block_number","params":{"op":"=","value":"0x4"}}"#,
        ] {
            let tree = format!(r#"{{"allOf":[{leaf},{{"anyOf":[{leaf}]}}]}}"#);
            let decoded = serde_json::from_str::<ValidityExpression>(&tree).unwrap();
            let flat = format!(r#"{{"anyOf":[[{leaf},{leaf}]]}}"#);
            assert_eq!(decoded, serde_json::from_str(&flat).unwrap());
        }
        for invalid in [
            r#"{"allOf":[]}"#,
            r#"{"allOf":[[]]}"#,
            r#"{"anyOf":[{"type":"block_number","params":{"op":"=","value":"0x1"}}],"allOf":[]}"#,
            r#"{"allOf":[{"type":"block_number","params":{"op":"=","value":"0x1"},"anyOf":[]}]}"#,
            r#"{"anyOf":[{"type":"block_number","type":"balance","params":{"op":"=","value":"0x1"}}]}"#,
        ] {
            assert!(serde_json::from_str::<ValidityExpression>(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn grouped_wire_is_bounded_across_all_branches() {
        let p = json!({"type":"block_number","params":{"op":"<=","value":"0x10"}});
        let group = vec![p.clone(); DEFAULT_MAX_VALIDITY_PREDICATES / 2];
        let full = json!({"anyOf":[group,group]});
        let expr: ValidityExpression = serde_json::from_value(full.clone()).unwrap();
        assert_eq!(serde_json::to_value(&expr).unwrap(), full);
        assert!(expr.validate(DEFAULT_MAX_VALIDITY_PREDICATES - 1).is_err());
        for invalid in [
            json!({"anyOf":[]}),
            json!({"anyOf":[[]]}),
            json!({"anyOf":[group,group,[p]]}),
            json!({"anyOf":[[p]],"all":[]}),
            json!({"allOf":[p,{}]}),
        ] {
            assert!(serde_json::from_value::<ValidityExpression>(invalid).is_err());
        }
    }
}
