use serde::{Deserialize, Serialize};

use super::{Codec, DeserializationError, SerializationError};

/// MessagePack codec.
///
/// See [rmp_serde] for details.
/// This serializes structures as maps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePack;

impl Codec for MessagePack {
    const NAME: &'static str = "MessagePack";

    fn serialize<Writer, Item>(mut writer: Writer, item: &Item) -> Result<(), super::SerializationError>
    where
        Writer: std::io::Write,
        Item: serde::Serialize,
    {
        rmp_serde::encode::write_named(&mut writer, item).map_err(SerializationError::new)
    }

    fn deserialize<Reader, Item>(reader: Reader) -> Result<Item, super::DeserializationError>
    where
        Reader: std::io::Read,
        Item: serde::de::DeserializeOwned,
    {
        rmp_serde::decode::from_read(reader).map_err(DeserializationError::new)
    }
}
