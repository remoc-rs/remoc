//! Serialized representation versioning.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser};

mod macros;

#[doc(inline)]
pub use crate::{impl_enum, impl_struct};

/// Decides between current and old version.
pub trait Versioner {
    /// Returns whether the old version should be used.
    ///
    /// If it returns `false`, the current version is used.
    /// If it returns `true`, the old version is used.
    fn use_old() -> Result<bool, String>;
}

/// Decides whether to use the old representation based on the capabilities of
/// the remote endpoint of the current connection.
///
/// If no remoc connection is active, the current representation is used.
#[doc(hidden)]
pub struct RemocCompact;

impl Versioner for RemocCompact {
    fn use_old() -> Result<bool, String> {
        #[cfg(feature = "rch")]
        if let Some(compact_transported) =
            crate::rch::base::with_storage(|storage| storage.remote_cfg().compact_transported)
        {
            return Ok(!compact_transported);
        }

        Ok(false)
    }
}

/// Type which has a current and old serializable representation.
pub trait Versioned
where
    Self: Sized,
{
    /// Decides whether to use the current or old version.
    type Versioner: Versioner;

    /// Current version reference type for serialization.
    type CurrentRef<'a>
    where
        Self: 'a;

    /// Get current version reference type for serialization.
    fn as_current<'a>(&'a self) -> Result<Self::CurrentRef<'a>, String>;

    /// Current version value type for deserialization.
    type Current;

    /// Transform from current version value type after deserialization.
    fn from_current(current: Self::Current) -> Result<Self, String>;

    /// Old version reference type for serialization.
    type OldRef<'a>
    where
        Self: 'a;

    /// Get old version reference type for serialization.
    fn as_old<'a>(&'a self) -> Result<Self::OldRef<'a>, String>;

    /// Old version value type for deserialization.
    type Old;

    /// Transform from old version value type after deserialization.
    fn from_old(old: Self::Old) -> Result<Self, String>;
}

/// Serializes a value that has a versioned serialized representation.
///
/// Depending on the [versioner](Versioned::Versioner) either the current or
/// the old representation is used.
///
/// The signature of this function and [deserialize] is compatible with
/// `#[serde(with = "...")]`.
pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Versioned,
    for<'a> T::CurrentRef<'a>: Serialize,
    for<'a> T::OldRef<'a>: Serialize,
    S: Serializer,
{
    match T::Versioner::use_old().map_err(ser::Error::custom)? {
        false => {
            let current_ref = value.as_current().map_err(ser::Error::custom)?;
            current_ref.serialize(serializer)
        }
        true => {
            let old_ref = value.as_old().map_err(ser::Error::custom)?;
            old_ref.serialize(serializer)
        }
    }
}

/// Deserializes a value that has a versioned serialized representation.
///
/// Depending on the [versioner](Versioned::Versioner) either the current or
/// the old representation is expected.
///
/// The signature of this function and [serialize] is compatible with
/// `#[serde(with = "...")]`.
pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Versioned,
    T::Current: Deserialize<'de>,
    T::Old: Deserialize<'de>,
    D: Deserializer<'de>,
{
    match T::Versioner::use_old().map_err(de::Error::custom)? {
        false => {
            let current = T::Current::deserialize(deserializer)?;
            T::from_current(current).map_err(de::Error::custom)
        }
        true => {
            let old = T::Old::deserialize(deserializer)?;
            T::from_old(old).map_err(de::Error::custom)
        }
    }
}
