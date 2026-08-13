use serde::{Deserialize, Serialize};

use super::{Codec, DeserializationError, SerializationError};

/// Postcard codec.
///
/// See [postcard] for details.
/// This uses the default function configuration.
#[derive(Clone, Serialize, Deserialize)]
pub struct Postcard;

impl Codec for Postcard {
    fn serialize<Writer, Item>(writer: Writer, item: &Item) -> Result<(), super::SerializationError>
    where
        Writer: std::io::Write,
        Item: serde::Serialize,
    {
        postcard::to_io(item, writer).map_err(SerializationError::new)?;
        Ok(())
    }

    fn deserialize<Reader, Item>(mut reader: Reader) -> Result<Item, super::DeserializationError>
    where
        Reader: std::io::Read,
        Item: serde::de::DeserializeOwned,
    {
        let mut bytes = vec![];
        reader.read_to_end(&mut bytes).map_err(DeserializationError::new)?;

        let item = postcard::from_bytes(&bytes).map_err(DeserializationError::new)?;
        Ok(item)
    }
}
