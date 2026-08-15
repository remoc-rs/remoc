use serde::{Deserialize, Serialize};

use super::{Codec, DeserializationError, SerializationError};

/// Compact representations.
///
/// This module contains types with a more compact serialized representation.
///
/// The representations serde provides for some types of the standard library
/// spell out struct field names and enum variant names, which is wasteful.
/// The types in this module avoid that by using unnamed fields, numerical
/// identifiers and, where applicable, a more efficient encoding of the value itself.
///
/// Its usage is completely optional.
///
/// ```rust
/// # use serde::{Serialize, Deserialize};
/// # use std::time::Duration;
/// #[derive(Serialize, Deserialize)]
/// pub struct MyData {
///     #[serde(rename = "_0")]
///     #[serde(with = "remoc::codec::compact")]
///     result: Result<u32, String>,
///     #[serde(rename = "_1")]
///     #[serde(with = "remoc::codec::compact")]
///     duration: Duration,
/// }
/// ```
pub mod compact {
    pub use postbag::compact::*;
}

/// # Fixed Size Integers
///
/// In some cases, the use of variably length encoded data may not be
/// preferable. These modules, for use with `#[serde(with = "remoc::codec::fixint")]`
/// "opt out" of variable length encoding.
///
/// Disables varint serialization/deserialization for the specified integer
/// field. The integer will always be serialized in the same way as a fixed
/// size array.
///
/// Support explicitly not provided for `usize` or `isize`, as
/// these types would not be portable between systems of different
/// pointer widths.
///
/// ```rust
/// # use serde::{Serialize, Deserialize};
/// #[derive(Serialize, Deserialize)]
/// pub struct DefinitelyFixInt {
///     #[serde(with = "remoc::codec::fixint")]
///     x: u16,
/// }
/// ```
pub mod fixint {
    pub use postbag::fixint::*;
}

/// The Postbag data format version to use on the current connection.
fn negotiated_version() -> Option<postbag::cfg::Version> {
    #[cfg(feature = "rch")]
    if let Some(remote) = crate::rch::base::with_storage(|storage| storage.remote_cfg().postbag_version) {
        return Some(remote.min(postbag::cfg::Version::default()));
    }

    if super::ALLOW_OUTSIDE_REMOC.get() {
        tracing::warn!("using local postbag version for tests");
        return Some(postbag::cfg::Version::default());
    }

    None
}

/// The configuration to use on the current connection.
fn cfg<const WITH_IDENTS: bool, const DEPTH_LIMIT: usize>() -> std::io::Result<postbag::cfg::Cfg<WITH_IDENTS>> {
    let Some(version) = negotiated_version() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "remoc codec can only be used inside remoc",
        ));
    };

    Ok(postbag::cfg::Cfg::<WITH_IDENTS>::new().with_depth_limit(DEPTH_LIMIT).with_version(version))
}

/// [Postbag codec](postbag) with the specified configuration.
///
/// Postbag is a high-performance binary codec that provides efficient data encoding
/// with configurable levels of forward and backward compatibility.
///
/// Normally you do not need to name this type directly; use the [`Postbag`] and
/// [`PostbagSlim`] type aliases instead, which also allow the nesting depth limit
/// to be specified, for example `Postbag<1024>`.
///
/// # Configuration
///
/// `WITH_IDENTS` selects whether struct field identifiers and enum variant identifiers
/// are serialized. See [`Postbag`] for the implications of enabling it and
/// [`PostbagSlim`] for the implications of disabling it.
///
/// `DEPTH_LIMIT` is the maximum nesting depth of serialized and deserialized data;
/// exceeding it fails with an error instead of overflowing the stack.
/// It defaults to [`postbag::cfg::DEFAULT_DEPTH_LIMIT`] and only needs to be changed
/// when transferring deeply nested, in particular recursive, data structures.
/// The depth limit is not part of the data format, thus both peers of a connection
/// may use different limits.
#[derive(Clone, Serialize, Deserialize)]
pub struct PostbagWith<const WITH_IDENTS: bool, const DEPTH_LIMIT: usize>;

impl<const WITH_IDENTS: bool, const DEPTH_LIMIT: usize> Codec for PostbagWith<WITH_IDENTS, DEPTH_LIMIT> {
    fn serialize<Writer, Item>(writer: Writer, item: &Item) -> Result<(), SerializationError>
    where
        Writer: std::io::Write,
        Item: serde::Serialize,
    {
        postbag::serialize(cfg::<WITH_IDENTS, DEPTH_LIMIT>()?, writer, item).map_err(SerializationError::new)?;
        Ok(())
    }

    fn deserialize<Reader, Item>(reader: Reader) -> Result<Item, DeserializationError>
    where
        Reader: std::io::Read,
        Item: serde::de::DeserializeOwned,
    {
        let value = postbag::deserialize(cfg::<WITH_IDENTS, DEPTH_LIMIT>()?, reader)
            .map_err(DeserializationError::new)?;
        Ok(value)
    }
}

/// [Postbag codec](postbag) for compact binary encoding with full forwards and backwards compatibility.
///
/// [`Postbag`] is a compact binary [serde] codec for Rust that keeps the Rust type system
/// fully intact and has full support for backwards and forwards compatibility.
///
/// ## Forward and Backward Compatibility
///
/// As usual a field a reader expects but does not receive takes its `#[serde(default)]`, and
/// a variant it does not know needs a `#[serde(other)]` fallback.
///
/// The following changes to your types are supported:
///
/// | Change to your types | **[`Postbag`]** | [`PostbagSlim`] |
/// | --- | --- | --- |
/// | **Structs** | | |
/// | Add a field | **anywhere** | at the end |
/// | Remove a field | **anywhere** | at the end |
/// | Rename a field | **when numbered** | always |
/// | Reorder fields | **yes** | no |
/// | **Enums** | | |
/// | Add a variant | **anywhere** | at the end |
/// | Remove a variant | **anywhere** | at the end |
/// | Rename a variant | **when numbered** | always |
/// | Reorder variants | yes | no |
/// | **Size** | **small** | even smaller |
///
/// ## Numerical Identifier Encoding
///
/// Struct fields and enum variants named `_0` through `_59` are encoded with just a
/// single byte instead of the full string identifier. Use `#[serde(rename = "...")]`
/// to specify the numerical id for each field or variant.
/// This can significantly reduce the size of transferred data for structs with many
/// fields and enums with long variant names:
///
/// ```rust
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct CompactData {
///     #[serde(rename = "_3")]
///     my_field: u32,
///     #[serde(rename = "_15")]
///     another_field: String,
///     // Regular field names work normally
///     normal_field: bool,
/// }
///
/// #[derive(Serialize, Deserialize)]
/// enum CompactEnum {
///     #[serde(rename = "_0")]
///     MyLongVariantName,
///     #[serde(rename = "_1")]
///     AnotherLongVariantName(u32),
///     #[serde(rename = "_2")]
///     YetAnotherVariant {
///         // Fields of struct variants can be numbered as well
///         #[serde(rename = "_0")]
///         my_field: u32,
///     },
///     // Regular variant names work normally
///     NormalVariant,
/// }
/// ```
///
/// This feature is entirely optional; regular field and variant names continue to work
/// as expected. Normal and numerical names can be mixed without limitations within a
/// single struct or enum.
///
/// Names that do not have the form `_n`, as well as ids of 60 and above, are encoded as
/// regular strings. Since the identifier determines compatibility, changing the id of a
/// field or variant is a breaking change, but fields and variants can be reordered freely.
///
/// #### Use with `std` types
///
/// The [`compact`] module enables numerical identifier encoding on common
/// types from the standard library, such as [`Result`](std::result::Result) and
/// [`Duration`](std::time::Duration).
///
/// ```rust
/// # use serde::{Serialize, Deserialize};
/// # use std::time::Duration;
/// #[derive(Serialize, Deserialize)]
/// pub struct MyData {
///     #[serde(rename = "_0")]
///     #[serde(with = "remoc::codec::compact")]
///     result: Result<u32, String>,
///     #[serde(rename = "_1")]
///     #[serde(with = "remoc::codec::compact")]
///     duration: Duration,
/// }
/// ```
///
/// ## Nesting Depth Limit
///
/// `DEPTH_LIMIT` specifies the maximum nesting depth of transferred data and defaults to
/// [`postbag::cfg::DEFAULT_DEPTH_LIMIT`]. Specify a higher limit when transferring deeply
/// nested data structures.
pub type Postbag<const DEPTH_LIMIT: usize = { postbag::cfg::DEFAULT_DEPTH_LIMIT }> =
    PostbagWith<true, DEPTH_LIMIT>;

/// [Postbag slim codec](postbag) for very compact binary encoding with limited forwards and backwards compatibility.
///
/// [`PostbagSlim`] is a very compact binary [serde] codec for Rust that keeps the Rust type system
/// fully intact and has limited support for backwards and forwards compatibility.
///
/// ## Forward and Backward Compatibility
///
/// As usual a field a reader expects but does not receive takes its `#[serde(default)]`, and
/// a variant it does not know needs a `#[serde(other)]` fallback.
///
/// The following changes to your types are supported:
///
/// | Change to your types | [`Postbag`] | **[`PostbagSlim`]** |
/// | --- | --- | --- |
/// | **Structs** | | |
/// | Add a field | anywhere | **at the end** |
/// | Remove a field | anywhere | **at the end** |
/// | Rename a field | when numbered | **always** |
/// | Reorder fields | yes | **no** |
/// | **Enums** | | |
/// | Add a variant | anywhere | **at the end** |
/// | Remove a variant | anywhere | **at the end** |
/// | Rename a variant | when numbered | **always** |
/// | Reorder variants | yes | **no** |
/// | **Size** | small | **even smaller** |
///
/// ## When to Use
///
/// Choose `PostbagSlim` when:
/// - Maximum performance and minimal serialized size are priorities
/// - Schema changes are infrequent or controlled
/// - Data structures have stable field ordering
///
/// Choose the full [`Postbag`] codec when schema flexibility is more important than
/// the slight size overhead.
///
/// ## Nesting Depth Limit
///
/// `DEPTH_LIMIT` specifies the maximum nesting depth of transferred data and defaults to
/// [`postbag::cfg::DEFAULT_DEPTH_LIMIT`]. Specify a higher limit when transferring deeply
/// nested data structures.
pub type PostbagSlim<const DEPTH_LIMIT: usize = { postbag::cfg::DEFAULT_DEPTH_LIMIT }> =
    PostbagWith<false, DEPTH_LIMIT>;
