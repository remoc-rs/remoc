use remoc::codec;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
};

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestEnum {
    One(u16),
    Two { field1: String, field2: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestStruct {
    simple: String,
    btree: BTreeMap<Vec<u8>, String>,
    hash: HashMap<(u16, String), u8>,
    enu: Vec<TestEnum>,
}

impl Default for TestStruct {
    fn default() -> Self {
        let mut data = Self {
            simple: "test_string".to_string(),
            btree: BTreeMap::new(),
            hash: HashMap::new(),
            enu: vec![TestEnum::One(11), TestEnum::Two { field1: "value1".to_string(), field2: 2 }],
        };
        data.btree.insert(vec![1, 2, 3], "first value".to_string());
        data.btree.insert(vec![4, 5, 6, 7], "second value".to_string());
        data.hash.insert((1, "one".to_string()), 10);
        data.hash.insert((2, "two".to_string()), 20);
        data.hash.insert((3, "three".to_string()), 30);
        data
    }
}

#[cfg(feature = "codec-json")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestStructWithAttr {
    simple: String,
    #[serde(with = "remoc::codec::map::btreemap")]
    btree: BTreeMap<Vec<u8>, String>,
    #[serde(with = "remoc::codec::map::hashmap")]
    hash: HashMap<(u16, String), u8>,
    enu: Vec<TestEnum>,
}

#[cfg(feature = "codec-json")]
impl Default for TestStructWithAttr {
    fn default() -> Self {
        let mut data = Self {
            simple: "test_string".to_string(),
            btree: BTreeMap::new(),
            hash: HashMap::new(),
            enu: vec![TestEnum::One(11), TestEnum::Two { field1: "value1".to_string(), field2: 2 }],
        };
        data.btree.insert(vec![1, 2, 3], "first value".to_string());
        data.btree.insert(vec![4, 5, 6, 7], "second value".to_string());
        data.hash.insert((1, "one".to_string()), 10);
        data.hash.insert((2, "two".to_string()), 20);
        data.hash.insert((3, "three".to_string()), 30);
        data
    }
}

#[allow(dead_code)]
fn roundtrip<T, Codec>()
where
    T: Default + Serialize + DeserializeOwned + fmt::Debug + Eq,
    Codec: codec::Codec,
{
    let data: T = Default::default();
    println!("data:\n{:?}", data);

    let mut buffer = Vec::new();
    <Codec as codec::Codec>::serialize(&mut buffer, &data).unwrap();
    println!("serialized ({} bytes):\n{}", buffer.len(), String::from_utf8_lossy(&buffer));

    let deser: T = <Codec as codec::Codec>::deserialize(buffer.as_slice()).unwrap();
    assert_eq!(deser, data);
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
fn postbag() {
    roundtrip::<TestStruct, codec::Postbag>()
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
fn postbag_slim() {
    roundtrip::<TestStruct, codec::PostbagSlim>()
}

#[cfg(feature = "codec-bincode")]
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
fn bincode() {
    roundtrip::<TestStruct, codec::Bincode>()
}

#[cfg(feature = "codec-ciborium")]
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
fn ciborium() {
    roundtrip::<TestStruct, codec::Ciborium>()
}

#[cfg(feature = "codec-json")]
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
#[should_panic]
fn json_without_attr() {
    roundtrip::<TestStruct, codec::Json>()
}

#[cfg(feature = "codec-json")]
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
fn json_with_attr() {
    roundtrip::<TestStructWithAttr, codec::Json>()
}

#[cfg(feature = "codec-message-pack")]
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
fn message_pack() {
    roundtrip::<TestStruct, codec::MessagePack>()
}

#[cfg(feature = "codec-postcard")]
#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
fn postcard() {
    roundtrip::<TestStruct, codec::Postcard>()
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
fn postbag_depth_limit() {
    use remoc::codec::{Codec, Postbag, PostbagSlim, PostbagWith};
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    enum Tree {
        Leaf,
        Node(Box<Tree>),
    }

    fn nested(depth: usize) -> Tree {
        let mut tree = Tree::Leaf;
        for _ in 0..depth {
            tree = Tree::Node(Box::new(tree));
        }
        tree
    }

    fn roundtrip<C: Codec>(value: &Tree) -> Result<Tree, String> {
        let mut buf = Vec::new();
        <C as Codec>::serialize(&mut buf, value).map_err(|err| err.to_string())?;
        <C as Codec>::deserialize(buf.as_slice()).map_err(|err| err.to_string())
    }

    // Default limit rejects deeply nested data.
    let deep = nested(200);
    assert!(roundtrip::<Postbag>(&deep).is_err());
    assert!(roundtrip::<PostbagSlim>(&deep).is_err());

    // Raised limit accepts it, through both the alias and the explicit form.
    assert_eq!(roundtrip::<Postbag<4096>>(&deep).unwrap(), deep);
    assert_eq!(roundtrip::<PostbagSlim<4096>>(&deep).unwrap(), deep);
    assert_eq!(roundtrip::<PostbagWith<true, 4096>>(&deep).unwrap(), deep);

    // Shallow data still works with the default codecs.
    let shallow = nested(4);
    assert_eq!(roundtrip::<Postbag>(&shallow).unwrap(), shallow);
    assert_eq!(roundtrip::<PostbagSlim>(&shallow).unwrap(), shallow);
}
