//! Wire representation of validity conditions and their transaction-wide decode limit.

use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, DeserializeSeed, SeqAccess, Visitor, value::MapAccessDeserializer},
};

use crate::{DEFAULT_MAX_VALIDITY_PREDICATES, ValidityPredicate, ValidityPredicateError};

/// A legacy conjunction, or an explicit disjunction of non-empty conjunctions.
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

struct Groups;
impl<'de> Visitor<'de> for Groups {
    type Value = Vec<Vec<ValidityPredicate>>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a non-empty list of non-empty validity groups")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut groups = Vec::new();
        let mut remaining = DEFAULT_MAX_VALIDITY_PREDICATES;
        while let Some(group) = seq.next_element_seed(PredicateList(remaining))? {
            if group.is_empty() {
                return Err(de::Error::custom("empty anyOf group"));
            }
            remaining -= group.len();
            groups.push(group);
        }
        if groups.is_empty() {
            return Err(de::Error::custom("empty anyOf"));
        }
        Ok(groups)
    }
}

fn deserialize_groups<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Vec<Vec<ValidityPredicate>>, D::Error> {
    d.deserialize_seq(Groups)
}

/// Reads the bounded legacy flat representation without buffering arbitrary JSON.
pub fn deserialize_bounded_predicates<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Vec<ValidityPredicate>, D::Error> {
    PredicateList(DEFAULT_MAX_VALIDITY_PREDICATES).deserialize(d)
}

impl<'de> Deserialize<'de> for ValidityExpression {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Expression;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Alternatives {
            #[serde(rename = "anyOf", deserialize_with = "deserialize_groups")]
            groups: Vec<Vec<ValidityPredicate>>,
        }
        impl<'de> Visitor<'de> for Expression {
            type Value = ValidityExpression;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a predicate list or an anyOf object")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                PredicateList(DEFAULT_MAX_VALIDITY_PREDICATES)
                    .visit_seq(seq)
                    .map(ValidityExpression::All)
            }
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                Ok(ValidityExpression::AnyOf(
                    Alternatives::deserialize(MapAccessDeserializer::new(map))?.groups,
                ))
            }
        }
        d.deserialize_any(Expression)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
            json!({"anyOf":[p]}),
        ] {
            assert!(serde_json::from_value::<ValidityExpression>(invalid).is_err());
        }
    }
}
