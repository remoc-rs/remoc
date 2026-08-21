//! Codecs for transforming values into and from binary wire format.
//!
//! Remoc uses the full **[Postbag codec](postbag::Postbag)** by default. It is
//! the recommended codec for most applications: it provides compact binary
//! encoding while allowing fields and enum variants to be added, removed, and
//! reordered as a protocol evolves.
//!
//! Other codecs are available for specialized requirements, such as
//! interoperating with an existing wire format, producing human-readable data,
//! or making a different size or performance trade-off. Selecting another codec
//! also selects its compatibility model; refer to that codec's documentation
//! before using it in a protocol that must evolve across software versions.
//!
//! All codecs implement [`Codec`] on top of [Serde](serde). A codec is part of
//! the wire protocol, so both endpoints must use the same one for a channel.
//!
//! # Selecting a codec
//!
//! The codec is a type parameter that defaults to [`Default`](tyalias@Default). Specify another
//! one when creating a channel or [connection](crate::Connect):
//!
//! ```
//! # use remoc::{codec, rch};
//! # tokio_test::block_on(async {
//! let (tx, rx) = rch::mpsc::channel::<u32, codec::PostbagSlim>(1);
//! # let _ = (tx, rx);
//! # });
//! ```
//!
//! Each channel carries its own codec, so channels over the same connection may
//! use different ones. [`set_codec`](crate::rch::mpsc::Sender::set_codec) changes
//! the codec of a channel half before it is sent to a remote endpoint.
//!
//! # Postbag compatibility
//!
//! Postbag Full includes identifiers and encoded lengths for fields and enum
//! variants. This lets a receiver skip unknown data and supports common schema
//! changes when the appropriate Serde attributes are used:
//!
//! * use `#[serde(default)]` whenever a receiver may expect a field that the
//!   sender omits;
//! * use `#[serde(other)]` to accept enum variants unknown to an older receiver;
//! * use stable numbered identifiers such as `#[serde(rename = "_0")]` when a
//!   field or variant may later be renamed.
//!
//! See [`Postbag`] for the complete compatibility table, recoverable fields,
//! numbered identifiers, and format limitations.
//!
//! # Crate features
//!
//! The [Postbag codecs](Postbag) are always available. Every other codec is gated by
//! its own `codec-*` crate feature; `full-codecs` enables all of them at once.
//!
//! # Transferring binary data efficiently
//!
//! [Serde](serde) treats `Vec<u8>` and `[u8; N]` like any other sequence and thus
//! serializes them element by element.
//! This is much slower than handling the data as one contiguous block and easily
//! becomes the bottleneck when transferring larger amounts of binary data.
//! This applies to every value that Remoc transfers.
//!
//! The most straightforward remedy is to use [`bytes::Bytes`] instead of `Vec<u8>`,
//! which serializes as a single byte block and additionally is cheap to clone:
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! use bytes::Bytes;
//!
//! #[derive(Serialize, Deserialize)]
//! struct Message {
//!     data: Bytes,
//! }
//! ```
//!
//! If you must keep a `Vec<u8>`, annotate the field with
//! [`serde_bytes`](https://docs.rs/serde_bytes) to get the same wire format and speed:
//!
//! ```
//! # use serde::{Serialize, Deserialize};
//! #[derive(Serialize, Deserialize)]
//! struct Message {
//!     #[serde(with = "serde_bytes")]
//!     data: Vec<u8>,
//! }
//! ```
//!

use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use std::{
    any::{Any, TypeId, type_name},
    cell::Cell,
    error::Error,
    fmt,
    io::{BufWriter, Read, Write},
    sync::Arc,
};

/// A cloneable, reference-counted error that is safe to share between tasks.
pub type ArcError = Arc<dyn Error + Send + Sync + 'static>;

/// An error consisting of a string message.
#[derive(Debug, Clone)]
pub(crate) struct ErrorMsg(pub String);

impl fmt::Display for ErrorMsg {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for ErrorMsg {}

/// Streaming serialization and deserialization is unavailable.
///
/// This is because the platform does not support threads or they
/// are not working.
///
/// When streaming is unavailable, only messages up to the size specified
/// in [`Cfg::max_data_size`](crate::chmux::Cfg::max_data_size) can be
/// sent and received. You can increase this limit to work around the issue.
#[derive(Debug, Clone)]
pub struct StreamingUnavailable;

impl fmt::Display for StreamingUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "streaming serialization and deserialization is unavailable")
    }
}

impl Error for StreamingUnavailable {}

/// Serialization error.
#[derive(Debug, Clone)]
pub struct SerializationError(pub ArcError);

impl SerializationError {
    /// Creates a new serialization error.
    pub fn new<E>(err: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self(Arc::new(err))
    }
}

impl From<std::io::Error> for SerializationError {
    fn from(err: std::io::Error) -> Self {
        Self(Arc::new(err))
    }
}

impl fmt::Display for SerializationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for SerializationError {}

impl Serialize for SerializationError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let msg = self.0.to_string();
        msg.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SerializationError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let msg = String::deserialize(deserializer)?;
        Ok(Self::new(ErrorMsg(msg)))
    }
}

/// Deserialization error.
#[derive(Debug, Clone)]
pub struct DeserializationError(pub ArcError);

impl DeserializationError {
    /// Creates a new deserialization error.
    pub fn new<E>(err: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self(Arc::new(err))
    }
}

impl From<std::io::Error> for DeserializationError {
    fn from(err: std::io::Error) -> Self {
        Self(Arc::new(err))
    }
}

impl fmt::Display for DeserializationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for DeserializationError {}

impl Serialize for DeserializationError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let msg = self.0.to_string();
        msg.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DeserializationError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let msg = String::deserialize(deserializer)?;
        Ok(Self::new(ErrorMsg(msg)))
    }
}

/// Serializes and deserializes items from and to byte data.
pub trait Codec: Send + Sync + Serialize + for<'de> Deserialize<'de> + Clone + Unpin + 'static {
    /// Serializes the specified item into the data format.
    fn serialize<Writer, Item>(writer: Writer, item: &Item) -> Result<(), SerializationError>
    where
        Writer: Write,
        Item: Serialize;

    /// Deserializes the specified data into an item.
    fn deserialize<Reader, Item>(reader: Reader) -> Result<Item, DeserializationError>
    where
        Reader: Read,
        Item: DeserializeOwned;
}

/// Dummy codec.
///
/// Does not support serialization or deserialization.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Dummy;

impl Codec for Dummy {
    fn serialize<Writer, Item>(_writer: Writer, _item: &Item) -> Result<(), SerializationError>
    where
        Writer: std::io::Write,
        Item: serde::Serialize,
    {
        Err(SerializationError::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "dummy codec does not support serialization",
        )))
    }

    fn deserialize<Reader, Item>(_reader: Reader) -> Result<Item, DeserializationError>
    where
        Reader: std::io::Read,
        Item: serde::de::DeserializeOwned,
    {
        Err(DeserializationError::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "dummy codec does not support deserialization",
        )))
    }
}

#[cfg(feature = "codec-json")]
pub mod map;

// ============================================================================
// Erased serializer and deserializer
// ============================================================================

/// Item that is Any and Send.
pub type AnySend = Box<dyn Any + Send>;

/// Type-erased serializer.
pub struct ErasedSerializer {
    type_id: TypeId,
    type_name: &'static str,
    codec_name: &'static str,
    inner: Box<dyn ErasedSerializerMethods>,
}

impl Clone for ErasedSerializer {
    fn clone(&self) -> Self {
        Self {
            type_id: self.type_id,
            type_name: self.type_name,
            codec_name: self.codec_name,
            inner: self.inner.clone(),
        }
    }
}

impl fmt::Debug for ErasedSerializer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ErasedSerializer")
            .field("type", &self.type_name)
            .field("codec", &self.codec_name)
            .finish()
    }
}

impl ErasedSerializer {
    /// Creates a new type-erased serializer for the given type and codec.
    pub fn new<T, C>() -> Self
    where
        T: Serialize + Any,
        C: Codec,
    {
        Self {
            type_id: TypeId::of::<T>(),
            type_name: type_name::<T>(),
            codec_name: type_name::<C>(),
            inner: Box::new(ErasedSerializerInner::<T, C>(|_, _| ())),
        }
    }

    /// Checks that the passed type matches the underlying type.
    #[track_caller]
    pub fn check_type(&self, item: &dyn Any) {
        if item.type_id() != self.type_id {
            panic!("expected type {} for serialization", self.type_name);
        }
    }

    /// Serialize the type-erased item into the given writer, passing it the encoded data
    /// in blocks of `buffer_size` bytes.
    ///
    /// # Panics
    /// Panics if the type of the item does not match the type `T` used for calling [`ErasedSerializer::new`].
    pub fn serialize(
        &self, writer: &mut dyn Write, item: &dyn Any, buffer_size: usize,
    ) -> Result<(), SerializationError> {
        self.inner.serialize(writer, item, buffer_size)
    }
}

trait ErasedSerializerMethods: Send + Sync {
    fn clone(&self) -> Box<dyn ErasedSerializerMethods>;
    fn serialize(
        &self, writer: &mut dyn Write, item: &dyn Any, buffer_size: usize,
    ) -> Result<(), SerializationError>;
}

#[expect(dead_code)]
struct ErasedSerializerInner<T, C>(fn(T, C));

impl<T, C> ErasedSerializerMethods for ErasedSerializerInner<T, C>
where
    T: Serialize + Any,
    C: Codec,
{
    fn clone(&self) -> Box<dyn ErasedSerializerMethods> {
        Box::new(ErasedSerializerInner::<T, C>(|_, _| ()))
    }

    fn serialize(
        &self, writer: &mut dyn Write, item: &dyn Any, buffer_size: usize,
    ) -> Result<(), SerializationError> {
        let Some(item) = item.downcast_ref::<T>() else { panic!("ErasedSerializer called with mismatched type") };

        let mut writer = BufWriter::with_capacity(buffer_size, writer);
        <C as Codec>::serialize(&mut writer, item)?;
        writer.flush().map_err(SerializationError::new)
    }
}

/// Type-erased deserializer.
pub struct ErasedDeserializer {
    type_name: &'static str,
    codec_name: &'static str,
    inner: Box<dyn ErasedDeserializerMethods>,
}

impl Clone for ErasedDeserializer {
    fn clone(&self) -> Self {
        Self { type_name: self.type_name, codec_name: self.codec_name, inner: self.inner.clone() }
    }
}

impl fmt::Debug for ErasedDeserializer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ErasedDeserializer")
            .field("type", &self.type_name)
            .field("codec", &self.codec_name)
            .finish()
    }
}

impl ErasedDeserializer {
    /// Creates a new type-erased deserializer for the given type and codec.
    pub fn new<T, C>() -> Self
    where
        T: DeserializeOwned + Any + Send,
        C: Codec,
    {
        Self {
            type_name: type_name::<T>(),
            codec_name: type_name::<C>(),
            inner: Box::new(ErasedDeserializerInner::<T, C>(|_, _| ())),
        }
    }

    /// Deserialize the item of type `T` used for calling [`ErasedDeserializer::new`] from the given reader.
    ///  
    /// The deserialized item is returned type erased.
    pub fn deserialize(&self, reader: &mut dyn Read) -> Result<AnySend, DeserializationError> {
        self.inner.deserialize(reader)
    }
}

trait ErasedDeserializerMethods: Send + Sync {
    fn clone(&self) -> Box<dyn ErasedDeserializerMethods>;
    fn deserialize(&self, reader: &mut dyn Read) -> Result<AnySend, DeserializationError>;
}

#[expect(dead_code)]
struct ErasedDeserializerInner<T, C>(fn(T, C));

impl<T, C> ErasedDeserializerMethods for ErasedDeserializerInner<T, C>
where
    T: DeserializeOwned + Any + Send,
    C: Codec,
{
    fn clone(&self) -> Box<dyn ErasedDeserializerMethods> {
        Box::new(ErasedDeserializerInner::<T, C>(|_, _| ()))
    }

    fn deserialize(&self, reader: &mut dyn Read) -> Result<AnySend, DeserializationError> {
        let item: T = <C as Codec>::deserialize(reader)?;
        Ok(Box::new(item))
    }
}

// ============================================================================
// Codecs
// ============================================================================

thread_local! {
    /// Allow using codecs outside of remoc (for testing only).
    #[doc(hidden)]
    pub static ALLOW_OUTSIDE_REMOC: Cell<bool> = const { Cell::new(false) };
}

mod postbag;
pub use postbag::{Postbag, PostbagSlim, PostbagWith, compact, fixint, recoverable, varfloat};

#[cfg(feature = "codec-bincode")]
mod bincode;
#[cfg(feature = "codec-bincode")]
pub use self::bincode::{Bincode, Bincode2};

#[cfg(feature = "codec-ciborium")]
mod ciborium;
#[cfg(feature = "codec-ciborium")]
pub use self::ciborium::Ciborium;

#[cfg(feature = "codec-json")]
mod json;
#[cfg(feature = "codec-json")]
pub use json::Json;

#[cfg(feature = "codec-message-pack")]
mod message_pack;
#[cfg(feature = "codec-message-pack")]
pub use message_pack::MessagePack;

#[cfg(feature = "codec-postcard")]
mod postcard;
#[cfg(feature = "codec-postcard")]
pub use postcard::Postcard;

#[allow(unused_macros)]
macro_rules! default_codec {
    ($codec:ident) => {
        #[doc = concat!("Default codec is overridden via crate feature to [", stringify!($codec), "].")]
        #[cfg_attr(
            not(feature = "default-codec-no-warn"),
            deprecated = "changing the default codec via the default-codec-* remoc feature is deprecated; \
                          you can enable the default-codec-no-warn remoc feature to disable this warning"
        )]
        pub type Default = $codec;
    };
}

// Select the default codec.
//
// Changing the default codec via Cargo features is deprecated.
cfg_select! {
    feature = "default-codec-postbag" => {
        #[doc(no_inline)]
        pub use postbag::Postbag as Default;
    }
    feature = "default-codec-postbag-slim" => {
        default_codec!(PostbagSlim);
    }
    feature = "default-codec-bincode" => {
        default_codec!(Bincode);
    }
    feature = "default-codec-bincode2" => {
        default_codec!(Bincode2);
    }
    feature = "default-codec-ciborium" => {
        default_codec!(Ciborium);
    }
    feature = "default-codec-json" => {
        default_codec!(Json);
    }
    feature = "default-codec-message-pack" => {
        default_codec!(MessagePack);
    }
    feature = "default-codec-postcard" => {
        default_codec!(Postcard);
    }
    _ => {
        #[doc(no_inline)]
        pub use postbag::Postbag as Default;
    }
}
