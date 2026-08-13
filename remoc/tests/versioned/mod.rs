//! Verifies that a user can implement versioning for their own types.

use remoc::{
    rch::base::{StorageRef, storage_ref},
    versioned::{self, Versioned, Versioner},
};
use serde::{Deserialize, Serialize};

#[cfg(all(target_family = "wasm", feature = "js"))]
use wasm_bindgen_test::wasm_bindgen_test;

use crate::loop_channel;

/// Protocol version of the application, stored in the connection storage.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ProtoVersion(u32);

/// Uses the old representation when the remote endpoint speaks version 1.
struct ProtoVersioner;

impl Versioner for ProtoVersioner {
    fn use_old() -> Result<bool, versioned::Error> {
        let version = remoc::rch::base::with_storage(|storage| storage.get::<ProtoVersion>())
            .flatten()
            .ok_or("protocol version is unknown")?;
        Ok(version.0 < 2)
    }
}

/// A user type whose representation changed between protocol versions.
///
/// Version 1 transferred the full name, version 2 transferred its parts
/// separately and added a new field.
#[derive(Debug, PartialEq)]
struct Person {
    first_name: String,
    last_name: String,
    age: u32,
}

#[derive(Serialize)]
struct PersonV2Ref<'a> {
    first_name: &'a String,
    last_name: &'a String,
    age: &'a u32,
}

#[derive(Deserialize)]
struct PersonV2 {
    first_name: String,
    last_name: String,
    age: u32,
}

#[derive(Serialize)]
struct PersonV1Ref {
    name: String,
}

#[derive(Deserialize)]
struct PersonV1 {
    name: String,
}

impl Versioned for Person {
    type Versioner = ProtoVersioner;

    type CurrentRef<'a> = PersonV2Ref<'a>;
    fn as_current<'a>(&'a self) -> Result<Self::CurrentRef<'a>, versioned::Error> {
        Ok(PersonV2Ref { first_name: &self.first_name, last_name: &self.last_name, age: &self.age })
    }

    type Current = PersonV2;
    fn from_current(current: Self::Current) -> Result<Self, versioned::Error> {
        Ok(Self { first_name: current.first_name, last_name: current.last_name, age: current.age })
    }

    type OldRef<'a> = PersonV1Ref;
    fn as_old<'a>(&'a self) -> Result<Self::OldRef<'a>, versioned::Error> {
        Ok(PersonV1Ref { name: format!("{} {}", self.first_name, self.last_name) })
    }

    type Old = PersonV1;
    fn from_old(old: Self::Old) -> Result<Self, versioned::Error> {
        let (first_name, last_name) = old.name.split_once(' ').ok_or("malformed name")?;
        // The age was not part of version 1.
        Ok(Self { first_name: first_name.to_string(), last_name: last_name.to_string(), age: 0 })
    }
}

remoc::impl_serde! { Person }

/// Message that establishes the protocol version and is never versioned itself.
#[derive(Serialize, Deserialize)]
struct Hello {
    version: u32,
    storage_ref: StorageRef,
}

#[derive(Serialize, Deserialize)]
enum Msg {
    Hello(Hello),
    Person(Person),
}

async fn exchange(remote_version: u32) -> Person {
    let ((mut a_tx, _), (_, mut b_rx)) = loop_channel::<Msg>().await;

    // The hello message is always backwards compatible and establishes the version.
    let (storage_ref, handle) = storage_ref();
    a_tx.send(Msg::Hello(Hello { version: remote_version, storage_ref })).await.unwrap();
    let sender_storage = handle.await.unwrap();
    sender_storage.insert(ProtoVersion(remote_version));

    let Msg::Hello(hello) = b_rx.recv().await.unwrap().unwrap() else { panic!("expected hello") };
    hello.storage_ref.get().unwrap().insert(ProtoVersion(hello.version));

    // Subsequent messages use the negotiated representation.
    a_tx.send(Msg::Person(Person { first_name: "Anna".to_string(), last_name: "Meyer".to_string(), age: 42 }))
        .await
        .unwrap();

    let Msg::Person(person) = b_rx.recv().await.unwrap().unwrap() else { panic!("expected person") };
    person
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn current_representation() {
    crate::init();

    let person = exchange(2).await;
    println!("version 2: {person:?}");
    assert_eq!(person, Person { first_name: "Anna".into(), last_name: "Meyer".into(), age: 42 });
}

#[cfg_attr(not(all(target_family = "wasm", feature = "js")), tokio::test)]
#[cfg_attr(all(target_family = "wasm", feature = "js"), wasm_bindgen_test)]
async fn old_representation() {
    crate::init();

    let person = exchange(1).await;
    println!("version 1: {person:?}");

    // Version 1 transferred the name as a whole and had no age field.
    assert_eq!(person, Person { first_name: "Anna".into(), last_name: "Meyer".into(), age: 0 });
}
