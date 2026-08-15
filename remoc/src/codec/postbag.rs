use serde::{Deserialize, Serialize};

use super::{Codec, DeserializationError, SerializationError};

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

/// [Postbag codec](postbag) with full forward and backward compatibility.
///
/// Postbag is a high-performance binary codec that provides efficient data encoding
/// with configurable levels of forward and backward compatibility. This codec uses the [`Full`](postbag::cfg::Full)
/// configuration which provides maximum compatibility and schema evolution capabilities.
///
/// ## Key Features
///
/// - **Full fidelity of Rust type system**: Supports all serde-compatible types including
///   structs, enums, tuples, arrays, maps, and all primitive types
/// - **Efficient binary format**: Uses variable-length encoding (varint) for integers,
///   compact representations for common types, and minimal overhead
/// - **Full forward/backward compatibility**: Fields and enum variants can be reordered,
///   added, or removed safely
///
/// ## Forward and Backward Compatibility
///
/// The `Full` configuration provides comprehensive schema evolution capabilities:
///
/// - **Field reordering**: Struct fields can be reordered without breaking compatibility
/// - **Field addition**: New fields can be added to structs at any position
/// - **Field removal**: Existing fields can be removed without affecting deserialization
/// - **Enum variant evolution**: Enum variants can be added, removed, or reordered
/// - **Schema evolution**: Safe evolution of data structures over time
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
/// This serializes and deserializes *with* field identifiers for maximum compatibility.
///
/// ## Compact Representations
///
/// The [`postbag::compact`] module provides more efficient representations of common
/// types from the standard library, such as [`Result`](std::result::Result),
/// [`Duration`](std::time::Duration) and [`SocketAddr`](std::net::SocketAddr), which
/// would otherwise spell out their field and variant names.
///
/// ## Nesting Depth Limit
///
/// `DEPTH_LIMIT` specifies the maximum nesting depth of transferred data and defaults to
/// [`postbag::cfg::DEFAULT_DEPTH_LIMIT`]. Specify a higher limit, for example
/// `Postbag<1024>`, when transferring deeply nested, in particular recursive, data
/// structures.
pub type Postbag<const DEPTH_LIMIT: usize = { postbag::cfg::DEFAULT_DEPTH_LIMIT }> =
    PostbagWith<true, DEPTH_LIMIT>;

/// [Postbag slim codec](postbag) for compact, high-performance encoding.
///
/// The [`Slim`](postbag::cfg::Slim) configuration prioritizes performance and compact size over compatibility.
/// This codec provides efficient binary encoding but with limited schema evolution
/// capabilities compared to the full [`Postbag`] codec.
///
/// ## Key Features
///
/// - **Compact encoding**: Smaller serialized data size compared to `Full` configuration
/// - **Fast processing**: No string lookups during serialization/deserialization
/// - **High performance**: Optimized for speed and minimal overhead
///
/// ## Schema Evolution Limitations
///
/// The `Slim` configuration has limited schema evolution capabilities. **Fields and enum
/// variants must maintain their order** for compatibility.
///
/// ### Supported Changes
///
/// - **Adding fields**: New fields can be added to the **end** of structs only
/// - **Removing fields**: Fields can be removed from the **end** of structs only
/// - **Adding enum variants**: New variants can be added at the **end** of enums only
/// - **Removing enum variants**: Variants can be removed from the **end** of enums only
///
/// ### Important Compatibility Notes
///
/// - Fields and enum variants **cannot be reordered**
/// - Fields and enum variants **cannot be added or removed from the middle**
/// - Use serde defaults (`#[serde(default)]`) for new fields to ensure backward compatibility
/// - Always add new fields at the end of struct definitions
/// - Always add new enum variants at the end of enum definitions
///
/// ## When to Use
///
/// Choose `PostbagSlim` when:
/// - Maximum performance and minimal serialized size are priorities
/// - Schema changes are infrequent or controlled
/// - Data structures have stable field ordering
///
/// Choose the full [`Postbag`] codec when schema flexibility is more important than
/// the slight performance overhead.
///
/// This serializes and deserializes *without* field identifiers for maximum efficiency.
///
/// ## Nesting Depth Limit
///
/// `DEPTH_LIMIT` specifies the maximum nesting depth of transferred data and defaults to
/// [`postbag::cfg::DEFAULT_DEPTH_LIMIT`]. Specify a higher limit, for example
/// `PostbagSlim<1024>`, when transferring deeply nested, in particular recursive, data
/// structures.
pub type PostbagSlim<const DEPTH_LIMIT: usize = { postbag::cfg::DEFAULT_DEPTH_LIMIT }> =
    PostbagWith<false, DEPTH_LIMIT>;
