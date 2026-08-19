//! Compact serialized representation of [`std::result::Result`].

/// Compact representation of [`std::result::Result`].
pub enum Result<T, E> {
    /// Contains the success value.
    Ok(T),
    /// Contains the error value.
    Err(E),
}

crate::versioned::compact::impl_enum! {
    Result<T, E>,
    variants {
        Ok(value: T) => "_0",
        Err(err: E) => "_1",
    }
    where
        T: ::serde::Serialize + ::serde::de::DeserializeOwned + 'static,
        E: ::serde::Serialize + ::serde::de::DeserializeOwned + 'static
}

impl<T, E> From<::std::result::Result<T, E>> for Result<T, E> {
    fn from(res: ::std::result::Result<T, E>) -> Self {
        match res {
            Ok(value) => Self::Ok(value),
            Err(err) => Self::Err(err),
        }
    }
}

impl<T, E> From<Result<T, E>> for ::std::result::Result<T, E> {
    fn from(res: Result<T, E>) -> Self {
        match res {
            Result::Ok(value) => Ok(value),
            Result::Err(err) => Err(err),
        }
    }
}
