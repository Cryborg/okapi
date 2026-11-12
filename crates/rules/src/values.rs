use std::collections::BTreeSet;
use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Deserializer};

use crate::error::ValidationError;

/// A text shown to the user, such as a rule name or a label: never blank.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Label(String);

impl Label {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Label {
    type Error = ValidationError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.trim().is_empty() {
            return Err(ValidationError::EmptyText);
        }
        Ok(Self(text))
    }
}

impl fmt::Display for Label {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The stable identifier of an application rule: lowercase letters, digits and
/// hyphens, starting with a letter or a digit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct RuleId(String);

impl RuleId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RuleId {
    type Error = ValidationError;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        let is_allowed =
            |character: char| character.is_ascii_lowercase() || character.is_ascii_digit();
        let mut characters = id.chars();
        let starts_well = characters.next().is_some_and(is_allowed);
        if starts_well && characters.all(|character| is_allowed(character) || character == '-') {
            Ok(Self(id))
        } else {
            Err(ValidationError::InvalidRuleId { id })
        }
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A list that holds at least one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    pub(crate) fn new(items: Vec<T>) -> Result<Self, ValidationError> {
        if items.is_empty() {
            Err(ValidationError::EmptyList)
        } else {
            Ok(Self(items))
        }
    }
}

impl<T> Deref for NonEmpty<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NonEmpty<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let items = Vec::deserialize(deserializer)?;
        Self::new(items).map_err(serde::de::Error::custom)
    }
}

/// A list without repeated items, possibly empty.
///
/// The check runs while the list is read, so `serde_json` reports the position
/// of the list that holds the duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueList<T>(Vec<T>);

impl<T> Deref for UniqueList<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<'de, T: Deserialize<'de> + Ord + fmt::Display> Deserialize<'de> for UniqueList<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let items: Vec<T> = Vec::deserialize(deserializer)?;
        match first_duplicate(&items) {
            Some(duplicate) => Err(serde::de::Error::custom(ValidationError::DuplicateValue {
                value: duplicate.to_string(),
            })),
            None => Ok(Self(items)),
        }
    }
}

/// The first item equal to an earlier one, if any.
pub(crate) fn first_duplicate<T: Ord>(items: impl IntoIterator<Item = T>) -> Option<T> {
    let mut seen = BTreeSet::new();
    items.into_iter().find_map(|item| {
        if seen.contains(&item) {
            Some(item)
        } else {
            seen.insert(item);
            None
        }
    })
}

/// Rejects `items` if two of them are equal; `field` names the list in the error.
pub(crate) fn ensure_unique<T: Ord + fmt::Display>(
    field: &'static str,
    items: &[T],
) -> Result<(), ValidationError> {
    match first_duplicate(items) {
        Some(duplicate) => Err(ValidationError::DuplicateEntry {
            field,
            value: duplicate.to_string(),
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_rejects_blank_text() {
        assert_eq!(
            Label::try_from("  ".to_owned()),
            Err(ValidationError::EmptyText)
        );
        assert_eq!(
            Label::try_from("Cache".to_owned()).unwrap().as_str(),
            "Cache"
        );
    }

    #[test]
    fn rule_id_accepts_lowercase_digits_and_hyphens() {
        for id in ["discord", "vs-code", "7zip", "a"] {
            assert_eq!(RuleId::try_from(id.to_owned()).unwrap().as_str(), id);
        }
    }

    #[test]
    fn rule_id_rejects_other_shapes() {
        for id in [
            "", "-discord", "Discord", "dIscord", "vs-Code", "vs_code", "vs code", "café",
        ] {
            assert_eq!(
                RuleId::try_from(id.to_owned()),
                Err(ValidationError::InvalidRuleId { id: id.to_owned() })
            );
        }
    }

    #[test]
    fn non_empty_rejects_an_empty_list() {
        assert_eq!(
            NonEmpty::<u8>::new(Vec::new()),
            Err(ValidationError::EmptyList)
        );
        assert_eq!(&*NonEmpty::new(vec![1, 2]).unwrap(), &[1, 2]);
        let error = serde_json::from_str::<NonEmpty<u8>>("[]").unwrap_err();
        assert!(error.to_string().starts_with("empty list"), "{error}");
    }

    #[test]
    fn a_unique_list_rejects_a_repeated_item_and_accepts_an_empty_list() {
        let error = serde_json::from_str::<UniqueList<u8>>("[1, 2, 1]").unwrap_err();
        assert!(
            error.to_string().starts_with("duplicate value `1`"),
            "{error}"
        );
        assert!(
            serde_json::from_str::<UniqueList<u8>>("[]")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn first_duplicate_is_the_first_item_seen_twice() {
        assert_eq!(first_duplicate(["A", "B", "B", "A"]), Some("B"));
        assert_eq!(first_duplicate(["A", "B"]), None);
    }

    #[test]
    fn ensure_unique_names_the_list_and_the_value() {
        assert_eq!(
            ensure_unique("targets", &["Cache", "GPUCache", "Cache"]),
            Err(ValidationError::DuplicateEntry {
                field: "targets",
                value: "Cache".to_owned()
            })
        );
        assert_eq!(ensure_unique("targets", &["Cache", "GPUCache"]), Ok(()));
    }
}
