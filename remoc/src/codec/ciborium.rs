use serde::{Deserialize, Serialize};

use super::{Codec, DeserializationError, SerializationError};

/// CBOR codec using [ciborium].
///
/// ## Compatibility
/// This codec is able to decode values encoded with Cbor
/// but the opposite is not true.
/// Make sure you are not mixing this codec with the legacy Cbor
/// codec across your remote endpoints to avoid deserialization errors.
/// More information is provided in the [`ciborium` README].
///
/// [`ciborium` README]: https://github.com/enarx/ciborium#compatibility-with-other-implementations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ciborium;

impl Codec for Ciborium {
    const NAME: &'static str = "Ciborium";

    fn serialize<Writer, Item>(writer: Writer, item: &Item) -> Result<(), super::SerializationError>
    where
        Writer: std::io::Write,
        Item: serde::Serialize,
    {
        ciborium::ser::into_writer(item, writer).map_err(SerializationError::new)
    }

    fn deserialize<Reader, Item>(reader: Reader) -> Result<Item, super::DeserializationError>
    where
        Reader: std::io::Read,
        Item: serde::de::DeserializeOwned,
    {
        ciborium::de::from_reader(reader).map_err(DeserializationError::new)
    }
}
