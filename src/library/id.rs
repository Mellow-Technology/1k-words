//! Ids for the catalog's rows: UUIDv7s, stored as 16-byte blobs.
//!
//! A catalog can be synced between devices (see `catalog`), each adding
//! rows while offline, so ids can't be counted up from one: they're random
//! enough never to collide. A UUIDv7 starts with the time it was made, so
//! ids sort by age, and rows made together sit together in the indexes.

use std::fmt;

use anyhow::{Result, bail};
use turso::Value;
use uuid::{NoContext, Timestamp, Uuid};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(Uuid);

impl Id {
    /// A new id, for now.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// A new id for a row made at `seconds` since the Unix epoch, as when
    /// giving rows ids that had none.
    pub fn at(seconds: f64) -> Self {
        let seconds = seconds.max(0.0);
        let timestamp = Timestamp::from_unix(
            NoContext,
            seconds.trunc() as u64,
            (seconds.fract() * 1e9) as u32,
        );
        Self(Uuid::new_v7(timestamp))
    }

    /// A number standing for the id, e.g. to tell elements apart in the
    /// UI: its random half, so unique in practice.
    pub fn key(&self) -> u64 {
        self.0.as_u64_pair().1
    }

    /// The id as an SQL blob literal, safe to put in a statement.
    pub(super) fn sql(&self) -> String {
        format!("X'{}'", self.0.simple())
    }

    pub(super) fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Blob(bytes) => match Uuid::from_slice(&bytes) {
                Ok(uuid) => Ok(Self(uuid)),
                Err(_) => bail!("an id of {} bytes", bytes.len()),
            },
            other => bail!("an id that isn't a blob: {other:?}"),
        }
    }

    pub(super) fn optional(value: Value) -> Result<Option<Self>> {
        match value {
            Value::Null => Ok(None),
            value => Self::from_value(value).map(Some),
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Uuid::parse_str(text).ok().map(Self)
    }

    #[cfg(test)]
    pub fn from_u128(value: u128) -> Self {
        Self(Uuid::from_u128(value))
    }
}

impl From<Id> for Value {
    fn from(id: Id) -> Self {
        Value::Blob(id.0.as_bytes().to_vec())
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.0)
    }
}

/// Ids for an SQL `IN (...)` list, as blob literals.
pub(super) fn sql_list(ids: &[Id]) -> String {
    ids.iter().map(Id::sql).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_sort_by_time_and_round_trip() {
        let (earlier, later) = (Id::at(1_700_000_000.0), Id::at(1_750_000_000.5));
        assert!(earlier < later);
        assert!(later < Id::new());
        assert_ne!(Id::at(5.0), Id::at(5.0));
        let value = Value::from(later);
        assert_eq!(Id::from_value(value).unwrap(), later);
        assert!(Id::from_value(Value::Integer(7)).is_err());
        assert_eq!(Id::optional(Value::Null).unwrap(), None);
        assert_eq!(Id::parse(&later.to_string()), Some(later));
        assert_eq!(later.sql().len(), 3 + 32);
    }
}
