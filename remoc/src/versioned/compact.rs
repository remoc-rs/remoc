//! Compaction of the serialized representation of Remoc's own types.

use super::{Error, Versioner};

#[doc(inline)]
pub use crate::{impl_enum, impl_struct};

/// Decides whether to use the old representation based on the capabilities of
/// the remote endpoint of the current connection.
///
/// If no remoc connection is active, the current representation is used.
pub struct CompactVersioner;

impl Versioner for CompactVersioner {
    fn use_old() -> Result<bool, Error> {
        #[cfg(feature = "rch")]
        if let Some(compact_transported) =
            crate::rch::base::with_storage(|storage| storage.remote_cfg().compact_transported)
        {
            return Ok(!compact_transported);
        }

        Ok(false)
    }
}

/// Implements a versioned serialized representation for a struct.
///
/// This generates the transported (serialized) representations of a struct
/// for the current and old version, implements
/// [Versioned](crate::versioned::Versioned) for it and implements
/// [Serialize](serde::Serialize) and [Deserialize](serde::Deserialize)
/// for it using these representations.
///
/// The generated representation types are private and thus invisible outside
/// of the generated code.
///
/// The current representation uses the specified serialization names, while the
/// old representation uses the field names of the struct.
///
/// # Syntax
///
/// The struct itself must be defined separately and must not derive
/// [Serialize](serde::Serialize) or [Deserialize](serde::Deserialize).
///
/// Generic parameters are specified after the struct name, with const generic
/// parameters following a semicolon, for example
/// `Receiver<T, Codec; const BUFFER: usize>`.
/// Bounds must be specified using the optional, trailing `where` clause.
///
/// ## Fields
///
/// The fields of the old representation are specified in the `fields` section,
/// in the order they appear in the old serialized representation.
///
///   * A field of the form `name: Type => "rename"` is taken from and stored into
///     the field `name` of the struct. It is present in both representations,
///     using the name `rename` in the current representation.
///   * A field of the form `name: Type = expr` is only present in the old
///     representation. `expr` provides its value for serialization and its
///     deserialized value is discarded.
///
/// Fields of the struct that are not transported must be listed in the optional
/// `default` section. They are initialized using [Default] when deserializing,
/// unless an initialization expression is specified using `field = expr`.
///
/// ## Field attributes
///
/// Serde attributes, for example `#[serde(default)]`, can be specified per field
/// and are applied to the generated representations.
///
/// A field can be prefixed with `#[compact]`, which must precede all other
/// attributes. Its value is then encoded using the compact representation
/// provided by [postbag::compact] in the current representation only.
///
/// # Example
///
/// ```ignore
/// pub struct LazyBlob<Codec = codec::Default> {
///     req_tx: mpsc::Sender<fw_bin::Sender, Codec, 1>,
///     len: u64,
///     fetch_task: Arc<Mutex<Option<FetchTask>>>,
/// }
///
/// impl_struct! {
///     LazyBlob<Codec>,
///     fields {
///         req_tx: mpsc::Sender<fw_bin::Sender, Codec, 1> => "_0",
///         len: u64 => "_1",
///     }
///     default { fetch_task }
///     where Codec: codec::Codec
/// }
/// ```
#[allow(unused_macros)]
#[doc(hidden)]
#[macro_export]
macro_rules! impl_struct {
    // Default value for a field that is not transported.
    (@default) => { ::std::default::Default::default() };
    (@default $init:expr) => { $init };

    (
        $name:ident $(< $( $gen:ident ),* $(,)? $(; $( const $cgen:ident : $cty:ty ),* $(,)? )? >)?,
        fields { $($fields:tt)* }
        $( default { $( $dfield:ident $(= $dinit:expr)? ),* $(,)? } )?
        $( where $($wc:tt)* )?
    ) => {
        $crate::impl_struct! { @munch
            name = [$name]
            generic_decls = [$( $($gen ,)* $( $(const $cgen : $cty ,)* )? )?]
            generic_args = [$( $($gen ,)* $( $($cgen ,)* )? )?]
            where_clause = [ $( where $($wc)* )? ]
            defaults = [ $( $( $dfield = $crate::impl_struct!(@default $($dinit)?) , )* )? ]
            rest = [ $($fields)* ]
            current_ref_fields = []
            current_fields = []
            old_ref_fields = []
            old_fields = []
            mapped = []
            old_extra_init = []
        }
    };

    // All fields consumed: emit the representations and the implementations.
    (@munch
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
        defaults = [$($dfield:ident = $dinit:expr),* $(,)?]
        rest = []
        current_ref_fields = [$($current_ref_fields:tt)*]
        current_fields = [$($current_fields:tt)*]
        old_ref_fields = [$($old_ref_fields:tt)*]
        old_fields = [$($old_fields:tt)*]
        mapped = [$($mapped:ident),* $(,)?]
        old_extra_init = [$($old_extra_init:tt)*]
    ) => {
        const _: () = {
            /// Reference to current transported representation for serialization.
            #[allow(private_interfaces)]
            #[derive(::serde::Serialize)]
            #[serde(bound = "")]
            pub struct CurrentRef<'transport, $($generic_decls)*>
            $($wc)*
            {
                $($current_ref_fields)*
                #[serde(skip)]
                _phantom: ::std::marker::PhantomData<&'transport ()>,
            }

            /// Current transported representation for deserialization.
            #[allow(private_interfaces)]
            #[derive(::serde::Deserialize)]
            #[serde(bound = "")]
            pub struct Current<$($generic_decls)*>
            $($wc)*
            {
                $($current_fields)*
            }

            /// Reference to old transported representation for serialization.
            #[allow(private_interfaces)]
            #[derive(::serde::Serialize)]
            #[serde(bound = "")]
            pub struct OldRef<'transport, $($generic_decls)*>
            $($wc)*
            {
                $($old_ref_fields)*
                #[serde(skip)]
                _phantom: ::std::marker::PhantomData<&'transport ()>,
            }

            /// Old transported representation for deserialization.
            #[allow(private_interfaces)]
            #[derive(::serde::Deserialize)]
            #[serde(bound = "")]
            pub struct Old<$($generic_decls)*>
            $($wc)*
            {
                $($old_fields)*
            }

        impl<$($generic_decls)*> $crate::versioned::Versioned for $name<$($generic_args)*>
        $($wc)*
        {
            type Versioner = $crate::versioned::compact::CompactVersioner;

            type CurrentRef<'transport>
                = CurrentRef<'transport, $($generic_args)*>
            where
                Self: 'transport;

            fn as_current<'transport>(
                &'transport self,
            ) -> ::std::result::Result<Self::CurrentRef<'transport>, $crate::versioned::Error> {
                ::std::result::Result::Ok(CurrentRef {
                    $( $mapped: &self.$mapped, )*
                    _phantom: ::std::marker::PhantomData,
                })
            }

            type Current = Current<$($generic_args)*>;

            fn from_current(
                current: Self::Current,
            ) -> ::std::result::Result<Self, $crate::versioned::Error> {
                ::std::result::Result::Ok(Self {
                    $( $mapped: current.$mapped, )*
                    $( $dfield: $dinit, )*
                })
            }

            type OldRef<'transport>
                = OldRef<'transport, $($generic_args)*>
            where
                Self: 'transport;

            fn as_old<'transport>(
                &'transport self,
            ) -> ::std::result::Result<Self::OldRef<'transport>, $crate::versioned::Error> {
                ::std::result::Result::Ok(OldRef {
                    $( $mapped: &self.$mapped, )*
                    $($old_extra_init)*
                    _phantom: ::std::marker::PhantomData,
                })
            }

            type Old = Old<$($generic_args)*>;

            fn from_old(old: Self::Old) -> ::std::result::Result<Self, $crate::versioned::Error> {
                ::std::result::Result::Ok(Self {
                    $( $mapped: old.$mapped, )*
                    $( $dfield: $dinit, )*
                })
            }
        }

        $crate::impl_serde! { @raw
            name = [$name]
            generic_decls = [$($generic_decls)*]
            generic_args = [$($generic_args)*]
            where_clause = [$($wc)*]
        }
        };
    };

    // Field present in both representations, using the compact representation
    // in the current representation.
    (@munch
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
        defaults = [$($dfield:ident = $dinit:expr),* $(,)?]
        rest = [ #[compact] $(#[$fattr:meta])* $field:ident : $fty:ty => $rename:literal, $($rest:tt)* ]
        current_ref_fields = [$($current_ref_fields:tt)*]
        current_fields = [$($current_fields:tt)*]
        old_ref_fields = [$($old_ref_fields:tt)*]
        old_fields = [$($old_fields:tt)*]
        mapped = [$($mapped:tt)*]
        old_extra_init = [$($old_extra_init:tt)*]
    ) => {
        $crate::impl_struct! { @munch
            name = [$name]
            generic_decls = [$($generic_decls)*]
            generic_args = [$($generic_args)*]
            where_clause = [$($wc)*]
            defaults = [$($dfield = $dinit,)*]
            rest = [ $($rest)* ]
            current_ref_fields = [
                $($current_ref_fields)*
                $(#[$fattr])*
                #[serde(rename = $rename)]
                #[serde(serialize_with = "postbag::compact::serialize")]
                $field: &'transport $fty,
            ]
            current_fields = [
                $($current_fields)*
                $(#[$fattr])*
                #[serde(rename = $rename)]
                #[serde(deserialize_with = "postbag::compact::deserialize")]
                $field: $fty,
            ]
            old_ref_fields = [ $($old_ref_fields)* $(#[$fattr])* $field: &'transport $fty, ]
            old_fields = [ $($old_fields)* $(#[$fattr])* $field: $fty, ]
            mapped = [ $($mapped)* $field, ]
            old_extra_init = [ $($old_extra_init)* ]
        }
    };

    // Field only present in old representation.
    (@munch
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
        defaults = [$($dfield:ident = $dinit:expr),* $(,)?]
        rest = [ $(#[$fattr:meta])* $field:ident : $fty:ty = $init:expr, $($rest:tt)* ]
        current_ref_fields = [$($current_ref_fields:tt)*]
        current_fields = [$($current_fields:tt)*]
        old_ref_fields = [$($old_ref_fields:tt)*]
        old_fields = [$($old_fields:tt)*]
        mapped = [$($mapped:tt)*]
        old_extra_init = [$($old_extra_init:tt)*]
    ) => {
        $crate::impl_struct! { @munch
            name = [$name]
            generic_decls = [$($generic_decls)*]
            generic_args = [$($generic_args)*]
            where_clause = [$($wc)*]
            defaults = [$($dfield = $dinit,)*]
            rest = [ $($rest)* ]
            current_ref_fields = [ $($current_ref_fields)* ]
            current_fields = [ $($current_fields)* ]
            old_ref_fields = [ $($old_ref_fields)* $(#[$fattr])* $field: $fty, ]
            old_fields = [ $($old_fields)* $(#[$fattr])* $field: $fty, ]
            mapped = [ $($mapped)* ]
            old_extra_init = [ $($old_extra_init)* $field: $init, ]
        }
    };

    // Field present in both representations.
    (@munch
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
        defaults = [$($dfield:ident = $dinit:expr),* $(,)?]
        rest = [ $(#[$fattr:meta])* $field:ident : $fty:ty => $rename:literal, $($rest:tt)* ]
        current_ref_fields = [$($current_ref_fields:tt)*]
        current_fields = [$($current_fields:tt)*]
        old_ref_fields = [$($old_ref_fields:tt)*]
        old_fields = [$($old_fields:tt)*]
        mapped = [$($mapped:tt)*]
        old_extra_init = [$($old_extra_init:tt)*]
    ) => {
        $crate::impl_struct! { @munch
            name = [$name]
            generic_decls = [$($generic_decls)*]
            generic_args = [$($generic_args)*]
            where_clause = [$($wc)*]
            defaults = [$($dfield = $dinit,)*]
            rest = [ $($rest)* ]
            current_ref_fields = [
                $($current_ref_fields)*
                $(#[$fattr])*
                #[serde(rename = $rename)]
                $field: &'transport $fty,
            ]
            current_fields = [
                $($current_fields)*
                $(#[$fattr])*
                #[serde(rename = $rename)]
                $field: $fty,
            ]
            old_ref_fields = [ $($old_ref_fields)* $(#[$fattr])* $field: &'transport $fty, ]
            old_fields = [ $($old_fields)* $(#[$fattr])* $field: $fty, ]
            mapped = [ $($mapped)* $field, ]
            old_extra_init = [ $($old_extra_init)* ]
        }
    };
}

/// Implements a versioned serialized representation for an enum.
///
/// This generates the transported (serialized) representations of an enum
/// for the current and old version, implements
/// [Versioned](crate::versioned::Versioned) for it and implements
/// [Serialize](serde::Serialize) and [Deserialize](serde::Deserialize)
/// for it using these representations.
///
/// The generated representation types are private and thus invisible outside
/// of the generated code.
///
/// The current representation uses the specified serialization names, while the
/// old representation uses the variant names of the enum.
///
/// Since a versioned enum implements [Serialize](serde::Serialize) and
/// [Deserialize](serde::Deserialize) by dispatching on the version itself, it can
/// be used as field type within other versioned representations.
///
/// # Syntax
///
/// The enum itself must be defined separately and must not derive
/// [Serialize](serde::Serialize) or [Deserialize](serde::Deserialize).
///
/// All variants must be listed in the `variants` section, in the order they appear
/// in the enum. Each field of a variant must be given a name, which is used
/// to reference it in the generated code. The name following `=>` is used as
/// variant name in the current representation.
/// Serde attributes can be specified per variant and are applied to the generated
/// representations.
///
/// A variant can be prefixed with `#[skip]`, which must precede all other
/// attributes and replaces the `=> "rename"` clause. Such a variant is part of
/// neither representation; attempting to serialize it results in an error.
///
/// Bounds required by the generated types and implementations must be specified
/// using the optional, trailing `where` clause.
///
/// At least one variant must have at least one field.
///
/// ## Recovery
///
/// The optional `recover` clause makes deserialization recoverable: a value that
/// cannot be deserialized, such as a variant only a newer version of the
/// application knows, is replaced by the specified expression instead of making
/// the enclosing value undecodable as well.
///
/// ```ignore
/// impl_enum! {
///     CallError,
///     recover = CallError::Remote(None),
///     variants {
///         Dropped => "_0",
///         Remote(err: Option<Box<CallError>>) => "_50",
///     }
/// }
/// ```
///
/// # Example
///
/// ```ignore
/// pub enum ListEvent<T> {
///     Push(T),
///     Done,
///     InitialComplete,
/// }
///
/// impl_enum! {
///     ListEvent<T>,
///     variants {
///         Push(value: T) => "_0",
///         Done => "_1",
///         #[skip]
///         InitialComplete,
///     }
///     where T: RemoteSend
/// }
/// ```
#[doc(hidden)]
#[macro_export]
macro_rules! impl_enum {
    // All variants are unit variants: no references and thus no lifetime are required.
    (
        $name:ident $(< $( $gen:ident ),* $(,)? $(; $( const $cgen:ident : $cty:ty ),* $(,)? )? >)?,
        $( recover = $recover:expr, )?
        variants {
            $( $(#[$vattr:meta])* $variant:ident => $rename:literal ),* $(,)?
        }
        $( where $($wc:tt)* )?
    ) => {
        const _: () = {
            /// Current transported representation.
            #[allow(private_interfaces)]
            #[derive(::serde::Serialize, ::serde::Deserialize)]
            #[serde(bound = "")]
            #[allow(clippy::enum_variant_names)]
            pub enum Current<$( $($gen ,)* $( $(const $cgen : $cty ,)* )? )?>
            $( where $($wc)* )?
            {
                $(
                    $(#[$vattr])*
                    #[serde(rename = $rename)]
                    $variant,
                )*
            }

            /// Old transported representation.
            #[allow(private_interfaces)]
            #[derive(::serde::Serialize, ::serde::Deserialize)]
            #[serde(bound = "")]
            #[allow(clippy::enum_variant_names)]
            pub enum Old<$( $($gen ,)* $( $(const $cgen : $cty ,)* )? )?>
            $( where $($wc)* )?
            {
                $(
                    $(#[$vattr])*
                    $variant,
                )*
            }

        impl<$( $($gen ,)* $( $(const $cgen : $cty ,)* )? )?> $crate::versioned::Versioned
            for $name<$( $($gen ,)* $( $($cgen ,)* )? )?>
        $( where $($wc)* )?
        {
            type Versioner = $crate::versioned::compact::CompactVersioner;

            type CurrentRef<'transport>
                = Current<$( $($gen ,)* $( $($cgen ,)* )? )?>
            where
                Self: 'transport;

            fn as_current<'transport>(
                &'transport self,
            ) -> ::std::result::Result<Self::CurrentRef<'transport>, $crate::versioned::Error> {
                ::std::result::Result::Ok(match self {
                    $( Self::$variant => Current::$variant, )*
                })
            }

            type Current = Current<$( $($gen ,)* $( $($cgen ,)* )? )?>;

            fn from_current(
                current: Self::Current,
            ) -> ::std::result::Result<Self, $crate::versioned::Error> {
                ::std::result::Result::Ok(match current {
                    $( Current::$variant => Self::$variant, )*
                })
            }

            type OldRef<'transport>
                = Old<$( $($gen ,)* $( $($cgen ,)* )? )?>
            where
                Self: 'transport;

            fn as_old<'transport>(
                &'transport self,
            ) -> ::std::result::Result<Self::OldRef<'transport>, $crate::versioned::Error> {
                ::std::result::Result::Ok(match self {
                    $( Self::$variant => Old::$variant, )*
                })
            }

            type Old = Old<$( $($gen ,)* $( $($cgen ,)* )? )?>;

            fn from_old(old: Self::Old) -> ::std::result::Result<Self, $crate::versioned::Error> {
                ::std::result::Result::Ok(match old {
                    $( Old::$variant => Self::$variant, )*
                })
            }
        }

        $crate::impl_serde! { @dispatch
            recover = [ $($recover)? ]
            name = [$name]
            generic_decls = [$( $($gen ,)* $( $(const $cgen : $cty ,)* )? )?]
            generic_args = [$( $($gen ,)* $( $($cgen ,)* )? )?]
            where_clause = [ $( where $($wc)* )? ]
        }
        };
    };

    // General case: at least one variant carries data.
    (
        $name:ident $(< $( $gen:ident ),* $(,)? $(; $( const $cgen:ident : $cty:ty ),* $(,)? )? >)?,
        $( recover = $recover:expr, )?
        variants { $($variants:tt)* }
        $( where $($wc:tt)* )?
    ) => {
        $crate::impl_enum! { @munch
            name = [$name]
            generic_decls = [$( $($gen ,)* $( $(const $cgen : $cty ,)* )? )?]
            generic_args = [$( $($gen ,)* $( $($cgen ,)* )? )?]
            where_clause = [ $( where $($wc)* )? ]
            recover = [ $($recover)? ]
            rest = [ $($variants)* ]
            current_ref_variants = []
            current_variants = []
            old_ref_variants = []
            old_variants = []
            as_current_arms = []
            from_current_arms = []
            as_old_arms = []
            from_old_arms = []
        }
    };

    // All variants consumed: emit the representations and the implementations.
    (@munch
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
        recover = [$($recover:tt)*]
        rest = []
        current_ref_variants = [$($current_ref_variants:tt)*]
        current_variants = [$($current_variants:tt)*]
        old_ref_variants = [$($old_ref_variants:tt)*]
        old_variants = [$($old_variants:tt)*]
        as_current_arms = [$($as_current_arms:tt)*]
        from_current_arms = [$($from_current_arms:tt)*]
        as_old_arms = [$($as_old_arms:tt)*]
        from_old_arms = [$($from_old_arms:tt)*]
    ) => {
        const _: () = {
            /// Reference to current transported representation for serialization.
            #[allow(private_interfaces)]
            #[derive(::serde::Serialize)]
            #[serde(bound = "")]
            #[allow(clippy::enum_variant_names)]
            pub enum CurrentRef<'transport, $($generic_decls)*>
            $($wc)*
            {
                $($current_ref_variants)*
            }

            /// Current transported representation for deserialization.
            #[allow(private_interfaces)]
            #[derive(::serde::Deserialize)]
            #[serde(bound = "")]
            #[allow(clippy::enum_variant_names)]
            pub enum Current<$($generic_decls)*>
            $($wc)*
            {
                $($current_variants)*
            }

            /// Reference to old transported representation for serialization.
            #[allow(private_interfaces)]
            #[derive(::serde::Serialize)]
            #[serde(bound = "")]
            #[allow(clippy::enum_variant_names)]
            pub enum OldRef<'transport, $($generic_decls)*>
            $($wc)*
            {
                $($old_ref_variants)*
            }

            /// Old transported representation for deserialization.
            #[allow(private_interfaces)]
            #[derive(::serde::Deserialize)]
            #[serde(bound = "")]
            #[allow(clippy::enum_variant_names)]
            pub enum Old<$($generic_decls)*>
            $($wc)*
            {
                $($old_variants)*
            }

        impl<$($generic_decls)*> $crate::versioned::Versioned for $name<$($generic_args)*>
        $($wc)*
        {
            type Versioner = $crate::versioned::compact::CompactVersioner;

            type CurrentRef<'transport>
                = CurrentRef<'transport, $($generic_args)*>
            where
                Self: 'transport;

            fn as_current<'transport>(
                &'transport self,
            ) -> ::std::result::Result<Self::CurrentRef<'transport>, $crate::versioned::Error> {
                ::std::result::Result::Ok(match self { $($as_current_arms)* })
            }

            type Current = Current<$($generic_args)*>;

            fn from_current(
                current: Self::Current,
            ) -> ::std::result::Result<Self, $crate::versioned::Error> {
                ::std::result::Result::Ok(match current { $($from_current_arms)* })
            }

            type OldRef<'transport>
                = OldRef<'transport, $($generic_args)*>
            where
                Self: 'transport;

            fn as_old<'transport>(
                &'transport self,
            ) -> ::std::result::Result<Self::OldRef<'transport>, $crate::versioned::Error> {
                ::std::result::Result::Ok(match self { $($as_old_arms)* })
            }

            type Old = Old<$($generic_args)*>;

            fn from_old(old: Self::Old) -> ::std::result::Result<Self, $crate::versioned::Error> {
                ::std::result::Result::Ok(match old { $($from_old_arms)* })
            }
        }

        $crate::impl_serde! { @dispatch
            recover = [$($recover)*]
            name = [$name]
            generic_decls = [$($generic_decls)*]
            generic_args = [$($generic_args)*]
            where_clause = [$($wc)*]
        }
        };
    };

    // Variant that is not part of the serialized representations.
    (@munch
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
        recover = [$($recover:tt)*]
        rest = [ #[skip] $(#[$vattr:meta])* $variant:ident, $($rest:tt)* ]
        current_ref_variants = [$($current_ref_variants:tt)*]
        current_variants = [$($current_variants:tt)*]
        old_ref_variants = [$($old_ref_variants:tt)*]
        old_variants = [$($old_variants:tt)*]
        as_current_arms = [$($as_current_arms:tt)*]
        from_current_arms = [$($from_current_arms:tt)*]
        as_old_arms = [$($as_old_arms:tt)*]
        from_old_arms = [$($from_old_arms:tt)*]
    ) => {
        $crate::impl_enum! { @munch
            name = [$name]
            generic_decls = [$($generic_decls)*]
            generic_args = [$($generic_args)*]
            where_clause = [$($wc)*]
            recover = [$($recover)*]
            rest = [ $($rest)* ]
            current_ref_variants = [ $($current_ref_variants)* ]
            current_variants = [ $($current_variants)* ]
            old_ref_variants = [ $($old_ref_variants)* ]
            old_variants = [ $($old_variants)* ]
            as_current_arms = [
                $($as_current_arms)*
                Self::$variant { .. } => {
                    return ::std::result::Result::Err(::std::convert::Into::into(::std::format!(
                        "the enum variant {}::{} cannot be serialized",
                        ::std::stringify!($name), ::std::stringify!($variant)
                    )))
                }
            ]
            from_current_arms = [ $($from_current_arms)* ]
            as_old_arms = [
                $($as_old_arms)*
                Self::$variant { .. } => {
                    return ::std::result::Result::Err(::std::convert::Into::into(::std::format!(
                        "the enum variant {}::{} cannot be serialized",
                        ::std::stringify!($name), ::std::stringify!($variant)
                    )))
                }
            ]
            from_old_arms = [ $($from_old_arms)* ]
        }
    };

    // Variant with named fields.
    (@munch
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
        recover = [$($recover:tt)*]
        rest = [
            $(#[$vattr:meta])*
            $variant:ident { $( $field:ident : $fty:ty ),+ $(,)? } => $rename:literal, $($rest:tt)*
        ]
        current_ref_variants = [$($current_ref_variants:tt)*]
        current_variants = [$($current_variants:tt)*]
        old_ref_variants = [$($old_ref_variants:tt)*]
        old_variants = [$($old_variants:tt)*]
        as_current_arms = [$($as_current_arms:tt)*]
        from_current_arms = [$($from_current_arms:tt)*]
        as_old_arms = [$($as_old_arms:tt)*]
        from_old_arms = [$($from_old_arms:tt)*]
    ) => {
        $crate::impl_enum! { @munch
            name = [$name]
            generic_decls = [$($generic_decls)*]
            generic_args = [$($generic_args)*]
            where_clause = [$($wc)*]
            recover = [$($recover)*]
            rest = [ $($rest)* ]
            current_ref_variants = [
                $($current_ref_variants)*
                $(#[$vattr])*
                #[serde(rename = $rename)]
                $variant { $( $field: &'transport $fty ),+ },
            ]
            current_variants = [
                $($current_variants)*
                $(#[$vattr])*
                #[serde(rename = $rename)]
                $variant { $( $field: $fty ),+ },
            ]
            old_ref_variants = [
                $($old_ref_variants)*
                $(#[$vattr])*
                $variant { $( $field: &'transport $fty ),+ },
            ]
            old_variants = [
                $($old_variants)*
                $(#[$vattr])*
                $variant { $( $field: $fty ),+ },
            ]
            as_current_arms = [
                $($as_current_arms)*
                Self::$variant { $($field),+ } => CurrentRef::$variant { $($field),+ },
            ]
            from_current_arms = [
                $($from_current_arms)*
                Current::$variant { $($field),+ } => Self::$variant { $($field),+ },
            ]
            as_old_arms = [
                $($as_old_arms)*
                Self::$variant { $($field),+ } => OldRef::$variant { $($field),+ },
            ]
            from_old_arms = [
                $($from_old_arms)*
                Old::$variant { $($field),+ } => Self::$variant { $($field),+ },
            ]
        }
    };

    // Variant with unnamed fields.
    (@munch
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
        recover = [$($recover:tt)*]
        rest = [
            $(#[$vattr:meta])*
            $variant:ident ( $( $field:ident : $fty:ty ),+ $(,)? ) => $rename:literal, $($rest:tt)*
        ]
        current_ref_variants = [$($current_ref_variants:tt)*]
        current_variants = [$($current_variants:tt)*]
        old_ref_variants = [$($old_ref_variants:tt)*]
        old_variants = [$($old_variants:tt)*]
        as_current_arms = [$($as_current_arms:tt)*]
        from_current_arms = [$($from_current_arms:tt)*]
        as_old_arms = [$($as_old_arms:tt)*]
        from_old_arms = [$($from_old_arms:tt)*]
    ) => {
        $crate::impl_enum! { @munch
            name = [$name]
            generic_decls = [$($generic_decls)*]
            generic_args = [$($generic_args)*]
            where_clause = [$($wc)*]
            recover = [$($recover)*]
            rest = [ $($rest)* ]
            current_ref_variants = [
                $($current_ref_variants)*
                $(#[$vattr])*
                #[serde(rename = $rename)]
                $variant ( $( &'transport $fty ),+ ),
            ]
            current_variants = [
                $($current_variants)*
                $(#[$vattr])*
                #[serde(rename = $rename)]
                $variant ( $( $fty ),+ ),
            ]
            old_ref_variants = [
                $($old_ref_variants)*
                $(#[$vattr])*
                $variant ( $( &'transport $fty ),+ ),
            ]
            old_variants = [
                $($old_variants)*
                $(#[$vattr])*
                $variant ( $( $fty ),+ ),
            ]
            as_current_arms = [
                $($as_current_arms)*
                Self::$variant ( $($field),+ ) => CurrentRef::$variant ( $($field),+ ),
            ]
            from_current_arms = [
                $($from_current_arms)*
                Current::$variant ( $($field),+ ) => Self::$variant ( $($field),+ ),
            ]
            as_old_arms = [
                $($as_old_arms)*
                Self::$variant ( $($field),+ ) => OldRef::$variant ( $($field),+ ),
            ]
            from_old_arms = [
                $($from_old_arms)*
                Old::$variant ( $($field),+ ) => Self::$variant ( $($field),+ ),
            ]
        }
    };

    // Variant without fields.
    (@munch
        name = [$name:ident]
        generic_decls = [$($generic_decls:tt)*]
        generic_args = [$($generic_args:tt)*]
        where_clause = [$($wc:tt)*]
        recover = [$($recover:tt)*]
        rest = [ $(#[$vattr:meta])* $variant:ident => $rename:literal, $($rest:tt)* ]
        current_ref_variants = [$($current_ref_variants:tt)*]
        current_variants = [$($current_variants:tt)*]
        old_ref_variants = [$($old_ref_variants:tt)*]
        old_variants = [$($old_variants:tt)*]
        as_current_arms = [$($as_current_arms:tt)*]
        from_current_arms = [$($from_current_arms:tt)*]
        as_old_arms = [$($as_old_arms:tt)*]
        from_old_arms = [$($from_old_arms:tt)*]
    ) => {
        $crate::impl_enum! { @munch
            name = [$name]
            generic_decls = [$($generic_decls)*]
            generic_args = [$($generic_args)*]
            where_clause = [$($wc)*]
            recover = [$($recover)*]
            rest = [ $($rest)* ]
            current_ref_variants = [
                $($current_ref_variants)*
                $(#[$vattr])*
                #[serde(rename = $rename)]
                $variant,
            ]
            current_variants = [
                $($current_variants)*
                $(#[$vattr])*
                #[serde(rename = $rename)]
                $variant,
            ]
            old_ref_variants = [ $($old_ref_variants)* $(#[$vattr])* $variant, ]
            old_variants = [ $($old_variants)* $(#[$vattr])* $variant, ]
            as_current_arms = [ $($as_current_arms)* Self::$variant => CurrentRef::$variant, ]
            from_current_arms = [ $($from_current_arms)* Current::$variant => Self::$variant, ]
            as_old_arms = [ $($as_old_arms)* Self::$variant => OldRef::$variant, ]
            from_old_arms = [ $($from_old_arms)* Old::$variant => Self::$variant, ]
        }
    };
}
