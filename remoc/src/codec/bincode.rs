use serde::{Deserialize, Serialize};

use super::{Codec, DeserializationError, SerializationError};

/// Bincode 1 codec.
///
/// See [bincode] for details.
/// This uses the [`bincode::config::legacy`] configuration.
#[derive(Clone, Serialize, Deserialize)]
pub struct Bincode;

const LEGACY: bincode::config::Configuration<
    bincode::config::LittleEndian,
    bincode::config::Fixint,
    bincode::config::NoLimit,
> = bincode::config::legacy();

impl Codec for Bincode {
    const NAME: &'static str = "Bincode";

    fn serialize<Writer, Item>(mut writer: Writer, item: &Item) -> Result<(), super::SerializationError>
    where
        Writer: std::io::Write,
        Item: serde::Serialize,
    {
        bincode::serde::encode_into_std_write(item, &mut writer, LEGACY)
            .map(|_| ())
            .map_err(SerializationError::new)
    }

    fn deserialize<Reader, Item>(mut reader: Reader) -> Result<Item, super::DeserializationError>
    where
        Reader: std::io::Read,
        Item: serde::de::DeserializeOwned,
    {
        bincode::serde::decode_from_std_read(&mut reader, LEGACY).map_err(DeserializationError::new)
    }
}

/// Bincode 2 codec.
///
/// See [bincode] for details.
/// This uses the [`bincode::config::standard`] configuration and is not compatible with Bincode 1.
#[derive(Clone, Serialize, Deserialize)]
pub struct Bincode2;

const STANDARD: bincode::config::Configuration = bincode::config::standard();

impl Codec for Bincode2 {
    const NAME: &'static str = "Bincode2";

    fn serialize<Writer, Item>(mut writer: Writer, item: &Item) -> Result<(), super::SerializationError>
    where
        Writer: std::io::Write,
        Item: serde::Serialize,
    {
        bincode::serde::encode_into_std_write(item, &mut writer, STANDARD)
            .map(|_| ())
            .map_err(SerializationError::new)
    }

    fn deserialize<Reader, Item>(mut reader: Reader) -> Result<Item, super::DeserializationError>
    where
        Reader: std::io::Read,
        Item: serde::de::DeserializeOwned,
    {
        bincode::serde::decode_from_std_read(&mut reader, STANDARD).map_err(DeserializationError::new)
    }
}
