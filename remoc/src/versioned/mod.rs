//! Serialized representation versioning.
//!
//! This module helps with evolving the serialized representation of a type
//! while staying compatible with remote endpoints that use an older version
//! of your application or library.
//!
//! # When is this needed?
//!
//! In most cases it is not. Additive changes are already handled by serde and
//! the codec: a field that is added can be marked with `#[serde(default)]` or
//! `#[serde(default = "...")]`, so that it is filled in when receiving data
//! from an endpoint that does not know it yet, and a previously used name can
//! be accepted using `#[serde(alias = "...")]`. Provided the codec transfers
//! field identifiers, for example the default [Postbag](crate::codec::Postbag)
//! codec, fields can be added, removed and reordered freely, because an
//! endpoint skips over fields it does not know.
//!
//! This covers roughly 90% of all schema changes and should be preferred,
//! since it requires no version information about the remote endpoint at all.
//!
//! Use this module when the data itself must be transformed, because the same
//! information is represented differently, for example when a field is split
//! up, when two fields are merged, when the type of a field changes or when a
//! new field is computed from previously transferred data. Such changes cannot
//! be expressed by serde attributes, since the value that must be sent to an
//! old endpoint does not exist as a field of your type anymore.
//!
//! # Usage
//!
//! Implement [Versioned] for your type. It defines two serialized
//! representations: the *current* one and an *old* one, together with the
//! conversions between them and your type. Both representations are
//! independent types, thus they may differ arbitrarily; fields can be added,
//! removed, renamed, restructured or change their type.
//!
//! Additionally provide a [Versioner], which decides for each value whether
//! the current or the old representation is used. Since the decision is made
//! per serialization, it can depend on the remote endpoint, for example by
//! consulting the version of the remote endpoint stored in the
//! [storage](crate::chmux::AnyStorage) of the connection.
//!
//! Then use [impl_serde] to implement [Serialize] and [Deserialize] for your
//! type in terms of these representations. Alternatively, use [serialize] and
//! [deserialize] directly, for example via `#[serde(with = "...")]` on a field
//! of another type.
//!
//! # Exchanging version information
//!
//! Remoc does not exchange application version information by itself, since
//! it cannot know how your versioning scheme looks like.
//!
//! Instead, your application or library should send a version message, the
//! serialized representation of which is never changed, over an established
//! connection.
//! Store the received version information into the
//! [storage](crate::chmux::AnyStorage) of the connection, which is accessible
//! during serialization and deserialization. Use
//! [StorageRef](crate::rch::base::StorageRef) to obtain the storage of a
//! connection when it is not otherwise available.
//!
//! Since Remoc preserves the order of messages within a channel, but not
//! between different channels, make sure that the exchange of version
//! information has been *confirmed* by the remote endpoint before sending
//! versioned data.
//!
//! Furthermore, most channels, for example [mpsc](crate::rch::mpsc), receive
//! and deserialize messages in a background task, thus a message that follows
//! the version message may already be deserialized before your code has
//! processed the version message and stored its information. The
//! [base channel](crate::rch::base) of a connection is an exception, since it
//! deserializes a message only when it is received by your code.
//!
//! Thus, to be safe over every channel, use the following exchange:
//!
//!   1. The connecting endpoint sends its version message and waits.
//!   2. The accepting endpoint receives it, stores the version information and
//!      replies with its own version message, but sends nothing else before.
//!   3. The connecting endpoint receives the reply, stores the version
//!      information and only then starts sending other messages.
//!
//!
//! # Serialization outside of Remoc
//!
//! [storage](crate::rch::base::storage) returns `None` when no serialization or
//! deserialization by Remoc is in progress. A [Versioner] can use this to
//! detect that a value is processed by another serializer, for example when it
//! is stored to disk, and select a different representation for that case.
//!
//! This is especially useful for a type that contains Remoc channels or other
//! remote objects, since these can only be serialized for sending over a
//! connection and fail otherwise. By providing a representation that contains
//! the data itself instead of the channels, such a type becomes storable.
//!
//! Note that the *old* representation then does not represent an older version
//! of your protocol, it is simply the alternative representation.
//!
//! ```
//! use remoc::{rch, versioned::{self, Versioner}};
//!
//! // Selects the old representation when the value is not serialized or
//! // deserialized by Remoc.
//! struct OutsideRemoc;
//!
//! impl Versioner for OutsideRemoc {
//!     fn use_old() -> Result<bool, versioned::Error> {
//!         Ok(rch::base::storage().is_none())
//!     }
//! }
//!
//! // Here no Remoc serialization or deserialization is in progress.
//! assert!(rch::base::storage().is_none());
//! assert!(OutsideRemoc::use_old().unwrap());
//! ```
//!
//! # Scope
//!
//! This module models exactly two representations of a type: the current one
//! and one old one. This covers evolving a type by one step and keeping
//! compatibility with the previous release.
//!
//! If you need to support more than two versions of a representation, either
//! nest [Versioned] implementations, i.e. let the old representation of a type
//! be a type that is itself versioned, or implement [Serialize] and
//! [Deserialize] directly and dispatch over the version of the remote
//! endpoint yourself.
//!
//! # Example
//!
//! Two endpoints negotiate their protocol version and then transfer a
//! `Person`. Version 1 of the protocol transferred the full name as a single
//! string, while version 2 transfers its parts separately and adds the age.
//!
//! Since the version is stored in the storage of the connection, `Person` can
//! be transferred over any Remoc channel, not just the one the version was
//! exchanged over.
//!
//! ```
//! use remoc::{rch, versioned::{self, Versioned, Versioner}};
//! use serde::{Deserialize, Serialize};
//!
//! // The protocol version of an endpoint.
//! #[derive(Debug, Clone, Copy, Serialize, Deserialize)]
//! struct ProtoVersion(u32);
//!
//! // The messages exchanged between the endpoints.
//! #[derive(Serialize, Deserialize)]
//! enum Msg {
//!     // Announces the protocol version of the sending endpoint.
//!     // Its serialized representation must never change.
//!     Version { version: ProtoVersion, storage: rch::base::StorageRef },
//!     // A person, transferred using the negotiated protocol version.
//!     Person(Person),
//! }
//!
//! // Exchanges the protocol version and stores the negotiated version into the
//! // storage of the connection, making it available during serialization and
//! // deserialization of every value that is transferred over that connection.
//! //
//! // Both endpoints send their version message immediately, which saves a
//! // round trip. This is only safe because a base channel deserializes a
//! // message when it is received; over other channels the accepting endpoint
//! // must reply to the version message of the connecting endpoint.
//! async fn handshake(
//!     our_version: ProtoVersion, tx: &mut rch::base::Sender<Msg>,
//!     rx: &mut rch::base::Receiver<Msg>,
//! ) {
//!     // StorageRef provides the storage of the connection it was received over.
//!     tx.send(Msg::Version { version: our_version, storage: rch::base::StorageRef::new() })
//!         .await.unwrap();
//!
//!     let Some(Msg::Version { version, storage }) = rx.recv().await.unwrap() else {
//!         panic!("expected version message");
//!     };
//!
//!     // Both endpoints use the highest version understood by both of them,
//!     // so that they arrive at the same decision.
//!     let negotiated = ProtoVersion(our_version.0.min(version.0));
//!     storage.get().unwrap().insert(negotiated);
//! }
//!
//! // Uses the old representation when protocol version 1 was negotiated.
//! struct ProtoVersioner;
//!
//! impl Versioner for ProtoVersioner {
//!     fn use_old() -> Result<bool, versioned::Error> {
//!         let version = rch::base::with_storage(|storage| storage.get::<ProtoVersion>())
//!             .flatten()
//!             .ok_or("protocol version has not been negotiated")?;
//!         Ok(version.0 < 2)
//!     }
//! }
//!
//! // The type that is transferred.
//! // It does not derive Serialize and Deserialize.
//! struct Person {
//!     first_name: String,
//!     last_name: String,
//!     age: u32,
//! }
//!
//! // Current representation, used with protocol version 2.
//! #[derive(Serialize)]
//! struct PersonV2Ref<'a> {
//!     first_name: &'a str,
//!     last_name: &'a str,
//!     age: u32,
//! }
//!
//! #[derive(Deserialize)]
//! struct PersonV2 {
//!     first_name: String,
//!     last_name: String,
//!     age: u32,
//! }
//!
//! // Old representation, used with protocol version 1.
//! #[derive(Serialize)]
//! struct PersonV1Ref {
//!     name: String,
//! }
//!
//! #[derive(Deserialize)]
//! struct PersonV1 {
//!     name: String,
//! }
//!
//! impl Versioned for Person {
//!     type Versioner = ProtoVersioner;
//!
//!     type CurrentRef<'a> = PersonV2Ref<'a>;
//!     fn as_current(&self) -> Result<Self::CurrentRef<'_>, versioned::Error> {
//!         Ok(PersonV2Ref {
//!             first_name: &self.first_name, last_name: &self.last_name, age: self.age,
//!         })
//!     }
//!
//!     type Current = PersonV2;
//!     fn from_current(current: Self::Current) -> Result<Self, versioned::Error> {
//!         Ok(Self {
//!             first_name: current.first_name, last_name: current.last_name, age: current.age,
//!         })
//!     }
//!
//!     type OldRef<'a> = PersonV1Ref;
//!     fn as_old(&self) -> Result<Self::OldRef<'_>, versioned::Error> {
//!         Ok(PersonV1Ref { name: format!("{} {}", self.first_name, self.last_name) })
//!     }
//!
//!     type Old = PersonV1;
//!     fn from_old(old: Self::Old) -> Result<Self, versioned::Error> {
//!         let (first_name, last_name) = old.name.split_once(' ').ok_or("malformed name")?;
//!
//!         // The age was not transferred by protocol version 1.
//!         Ok(Self {
//!             first_name: first_name.to_string(), last_name: last_name.to_string(), age: 0,
//!         })
//!     }
//! }
//!
//! // Person can now be sent over any Remoc channel.
//! versioned::impl_serde! { Person }
//!
//! // This endpoint implements protocol version 2.
//! async fn new_endpoint(mut tx: rch::base::Sender<Msg>, mut rx: rch::base::Receiver<Msg>) {
//!     handshake(ProtoVersion(2), &mut tx, &mut rx).await;
//!
//!     let person = Person {
//!         first_name: "Alice".to_string(), last_name: "Anderson".to_string(), age: 30,
//!     };
//!     tx.send(Msg::Person(person)).await.unwrap();
//! }
//!
//! // This endpoint implements protocol version 1, thus both endpoints
//! // negotiate that version and the old representation is transferred.
//! async fn old_endpoint(mut tx: rch::base::Sender<Msg>, mut rx: rch::base::Receiver<Msg>) {
//!     handshake(ProtoVersion(1), &mut tx, &mut rx).await;
//!
//!     let Some(Msg::Person(person)) = rx.recv().await.unwrap() else {
//!         panic!("expected person message");
//!     };
//!     assert_eq!(person.first_name, "Alice");
//!     assert_eq!(person.last_name, "Anderson");
//!
//!     // The age is not transferred by protocol version 1.
//!     assert_eq!(person.age, 0);
//! }
//! # tokio_test::block_on(remoc::doctest::client_server_bidir(new_endpoint, old_endpoint));
//! ```

use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser};

#[doc(hidden)]
pub mod compact;

#[doc(hidden)]
pub mod result;

#[doc(inline)]
pub use crate::impl_serde;

/// Error that occurred while converting between a value and its
/// current or old serialized representation.
///
/// It is converted into an error of the [Serializer] or [Deserializer]
/// using its [Display](std::fmt::Display) implementation.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Decides between current and old version.
pub trait Versioner {
    /// Returns whether the old version should be used.
    ///
    /// If it returns `false`, the current version is used.
    /// If it returns `true`, the old version is used.
    fn use_old() -> Result<bool, Error>;
}

/// Type which has a current and old serializable representation.
///
/// See the [module level documentation](self) for an example.
pub trait Versioned
where
    Self: Sized,
{
    /// Decides whether to use the current or old version.
    type Versioner: Versioner;

    /// Current version reference type for serialization.
    type CurrentRef<'a>
    where
        Self: 'a;

    /// Get current version reference type for serialization.
    fn as_current<'a>(&'a self) -> Result<Self::CurrentRef<'a>, Error>;

    /// Current version value type for deserialization.
    type Current;

    /// Transform from current version value type after deserialization.
    fn from_current(current: Self::Current) -> Result<Self, Error>;

    /// Old version reference type for serialization.
    type OldRef<'a>
    where
        Self: 'a;

    /// Get old version reference type for serialization.
    fn as_old<'a>(&'a self) -> Result<Self::OldRef<'a>, Error>;

    /// Old version value type for deserialization.
    type Old;

    /// Transform from old version value type after deserialization.
    fn from_old(old: Self::Old) -> Result<Self, Error>;
}

/// Serializes a value that has a versioned serialized representation.
///
/// Depending on the [versioner](Versioned::Versioner) either the current or
/// the old representation is used.
///
/// Can also be used with `#[serde(with = "remoc::versioned")]`.
pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Versioned,
    for<'a> T::CurrentRef<'a>: Serialize,
    for<'a> T::OldRef<'a>: Serialize,
    S: Serializer,
{
    match T::Versioner::use_old().map_err(ser::Error::custom)? {
        false => {
            let current_ref = value.as_current().map_err(ser::Error::custom)?;
            current_ref.serialize(serializer)
        }
        true => {
            let old_ref = value.as_old().map_err(ser::Error::custom)?;
            old_ref.serialize(serializer)
        }
    }
}

/// Deserializes a value that has a versioned serialized representation.
///
/// Depending on the [versioner](Versioned::Versioner) either the current or
/// the old representation is expected.
///
/// Can also be used with `#[serde(with = "remoc::versioned")]`.
pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Versioned,
    T::Current: Deserialize<'de>,
    T::Old: Deserialize<'de>,
    D: Deserializer<'de>,
{
    match T::Versioner::use_old().map_err(de::Error::custom)? {
        false => {
            let current = T::Current::deserialize(deserializer)?;
            T::from_current(current).map_err(de::Error::custom)
        }
        true => {
            let old = T::Old::deserialize(deserializer)?;
            T::from_old(old).map_err(de::Error::custom)
        }
    }
}

/// Implements [Serialize](serde::Serialize) and [Deserialize](serde::Deserialize)
/// for a type that implements [Versioned](crate::versioned::Versioned).
///
/// The serialized representation is chosen by the
/// [versioner](crate::versioned::Versioned::Versioner) of the type.
///
/// # Syntax
///
/// Generic parameters are specified after the type name, with const generic
/// parameters following a semicolon, for example
/// `Message<T, Codec; const BUFFER: usize>`.
/// Bounds must be specified using the optional, trailing `where` clause.
///
/// # Example
///
/// ```ignore
/// struct Message<T> {
///     data: T,
/// }
///
/// impl<T> Versioned for Message<T> where T: RemoteSend {
///     // ...
/// }
///
/// impl_serde! {
///     Message<T>
///     where T: RemoteSend
/// }
/// ```
#[doc(hidden)]
#[macro_export]
macro_rules! impl_serde {
    (
        $name:ident $(< $( $gen:ident ),* $(,)? $(; $( const $cgen:ident : $cty:ty ),* $(,)? )? >)?
        $( where $($wc:tt)* )?
    ) => {
        $crate::impl_serde! { @raw
            name = [$name]
            generic_decls = [$( $($gen ,)* $( $(const $cgen : $cty ,)* )? )?]
            generic_args = [$( $($gen ,)* $( $($cgen ,)* )? )?]
            where_clause = [ $( where $($wc)* )? ]
        }
    };

    // Dispatch on whether a recovery fallback was specified.
    (@dispatch
        recover = []
        $($rest:tt)*
    ) => {
        $crate::impl_serde! { @raw $($rest)* }
    };

    (@dispatch
        recover = [$($recover:tt)+]
        $($rest:tt)*
    ) => {
        $crate::impl_serde! { @recoverable recover = [$($recover)+] $($rest)* }
    };

    // Implements Serialize and Deserialize so that a failure to deserialize the
    // value is confined to it and replaced by the specified fallback.
    //
    // The wrapper cannot be applied to the type itself, since deserializing a
    // recoverable value deserializes the contained type, which would lead back
    // here and recurse without end. Thus it is applied to private types that
    // perform the actual conversion.
    (@recoverable
        recover = [$($recover:tt)+]
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
    ) => {
        const _: () = {
            /// Serializes the value without the recovery wrapper.
            struct PlainRef<'transport, $($generic_decls)*>(&'transport $name<$($generic_args)*>)
            $($wc)*;

            impl<'transport, $($generic_decls)*> ::serde::Serialize
                for PlainRef<'transport, $($generic_args)*>
            $($wc)*
            {
                fn serialize<Ser>(&self, serializer: Ser) -> ::std::result::Result<Ser::Ok, Ser::Error>
                where
                    Ser: ::serde::Serializer,
                {
                    $crate::versioned::serialize(self.0, serializer)
                }
            }

            /// Deserializes the value without the recovery wrapper.
            struct Plain<$($generic_decls)*>($name<$($generic_args)*>)
            $($wc)*;

            impl<'de, $($generic_decls)*> ::serde::Deserialize<'de> for Plain<$($generic_args)*>
            $($wc)*
            {
                fn deserialize<De>(deserializer: De) -> ::std::result::Result<Self, De::Error>
                where
                    De: ::serde::Deserializer<'de>,
                {
                    ::std::result::Result::Ok(Plain($crate::versioned::deserialize(deserializer)?))
                }
            }

            /// Provides the value to use when deserialization failed.
            struct Fallback;

            impl<$($generic_decls)*> $crate::codec::recoverable::Recover<Plain<$($generic_args)*>>
                for Fallback
            $($wc)*
            {
                fn recover<E>(_err: E) -> ::std::result::Result<Plain<$($generic_args)*>, E>
                where
                    E: ::serde::de::Error,
                {
                    ::std::result::Result::Ok(Plain($($recover)+))
                }
            }

            impl<$($generic_decls)*> ::serde::Serialize for $name<$($generic_args)*>
            $($wc)*
            {
                fn serialize<Ser>(&self, serializer: Ser) -> ::std::result::Result<Ser::Ok, Ser::Error>
                where
                    Ser: ::serde::Serializer,
                {
                    let value = $crate::codec::recoverable::Recoverable::<_, Fallback>::new(
                        PlainRef(self)
                    );
                    ::serde::Serialize::serialize(&value, serializer)
                }
            }

            impl<'de, $($generic_decls)*> ::serde::Deserialize<'de> for $name<$($generic_args)*>
            $($wc)*
            {
                fn deserialize<De>(deserializer: De) -> ::std::result::Result<Self, De::Error>
                where
                    De: ::serde::Deserializer<'de>,
                {
                    type Wrapped<$($generic_decls)*> = $crate::codec::recoverable::Recoverable<
                        Plain<$($generic_args)*>, Fallback
                    >;
                    let value = <Wrapped<$($generic_args)*> as ::serde::Deserialize>::deserialize(
                        deserializer
                    )?;
                    ::std::result::Result::Ok(
                        $crate::codec::recoverable::Recoverable::into_inner(value).0
                    )
                }
            }
        };
    };

    (@raw
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
    ) => {
        impl<$($generic_decls)*> ::serde::Serialize for $name<$($generic_args)*>
        $($wc)*
        {
            fn serialize<Ser>(&self, serializer: Ser) -> ::std::result::Result<Ser::Ok, Ser::Error>
            where
                Ser: ::serde::Serializer,
            {
                $crate::versioned::serialize(self, serializer)
            }
        }

        impl<'de, $($generic_decls)*> ::serde::Deserialize<'de> for $name<$($generic_args)*>
        $($wc)*
        {
            fn deserialize<De>(deserializer: De) -> ::std::result::Result<Self, De::Error>
            where
                De: ::serde::Deserializer<'de>,
            {
                $crate::versioned::deserialize(deserializer)
            }
        }
    };
}
