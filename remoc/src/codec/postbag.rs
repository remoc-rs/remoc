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

/// Recoverable deserialization.
///
/// When a value fails to deserialize, the error normally aborts deserialization
/// of the whole data structure, because a deserializer cannot know where the
/// undecodable value ends and thus cannot continue after it. Consequently, a
/// change to a type that breaks forward compatibility
/// renders every enclosing value undecodable as well, even when the enclosing
/// type itself did not change.
///
/// A value annotated with `#[serde(with = "remoc::codec::recoverable")]` is
/// deserialized in isolation, so that a failure is confined to it.
/// The rest of the enclosing data structure is deserialized as usual and the
/// value itself is replaced by one obtained from [`Default::default`].
///
/// ```rust
/// # use serde::{Serialize, Deserialize};
/// # #[derive(Default, Serialize, Deserialize)]
/// # struct B { x: u32 }
/// # #[derive(Default, Serialize, Deserialize)]
/// # struct C { x: u32 }
/// #[derive(Serialize, Deserialize)]
/// struct A {
///     b: B,
///     #[serde(with = "remoc::codec::recoverable")]
///     c: C,
/// }
/// ```
///
/// See [`postbag::recoverable`] for more details and options.
pub mod recoverable {
    pub use postbag::recoverable::*;
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

/// # Variable Size Floats
///
/// In some cases, the use of fixed size floating point data may be wasteful.
/// These modules, for use with `#[serde(with = "remoc::codec::varfloat")]` "opt in"
/// to variable length encoding.
///
/// Enables variable length serialization/deserialization for the specified
/// floating point field. The encoding is lossless and preserves the bit
/// pattern of every value, including quiet and signaling NaN payloads,
/// both infinities, negative zero and subnormal values.
///
/// Whether this saves space depends entirely on the data:
///
/// | Value | `f64` bytes | `f32` bytes |
/// | --- | ---: | ---: |
/// | `0.0` | 1 | 1 |
/// | `-0.0` | 2 | 2 |
/// | `1.0` | 3 | 3 |
/// | `-0.5` | 3 | 2 |
/// | `INFINITY` | 3 | 3 |
/// | `NAN` | 3 | 3 |
/// | `1234.0 / 32768.0` | 4 | 4 |
/// | `0.1` | 9 | 5 |
/// | `PI` | 9 | 5 |
/// | unencoded | 8 | 4 |
///
/// So this is worth applying to values that carry fewer significant bits than
/// their type provides, such as data quantized to a power of two, values that
/// are whole numbers, and fields that are zero most of the time.
///
/// ```rust
/// # use serde::Serialize;
/// #[derive(Serialize)]
/// pub struct DefinitelyVarfloat {
///     #[serde(with = "remoc::codec::varfloat")]
///     x: f64,
/// }
/// ```
pub mod varfloat {
    pub use postbag::varfloat::*;
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

    Ok(postbag::cfg::Cfg::<WITH_IDENTS>::new()
        .with_depth_limit(DEPTH_LIMIT)
        .with_version(version)
        .with_header(false))
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
/// ## Recoverable deserialization
///
/// Postbag can replace fields that failed to deserialize due to incompatible schema changes
/// with their default values and continue deserializing the remaining fields.
/// See [`recoverable`] how to use this.
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
