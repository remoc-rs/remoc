//! Associated types declared in a remote trait.
//!
//! On generated artifacts (request enums, client, request receiver) the
//! associated types are lifted to bare type parameters.
//! The lifted parameter name is always prefixed with `__`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Attribute, Ident, Token, Type, TypeParamBound, TypePath, parse::ParseStream, punctuated::Punctuated,
    visit_mut::VisitMut,
};

use crate::util::attribute_tokens;

/// Associated type declared in a remote trait.
#[derive(Debug, Clone)]
pub struct AssocType {
    /// Attributes on the associated type.
    pub attrs: Vec<Attribute>,
    /// Identifier as written by the user, e.g. `Item`.
    pub ident: Ident,
    /// Bounds, e.g. `RemoteSend + Clone`.
    pub bounds: Punctuated<TypeParamBound, Token![+]>,
}

impl AssocType {
    /// Identifier used when the associated type is lifted to a real
    /// generic parameter on generated artifacts. Always prefixed with
    /// `__`.
    pub fn lifted_ident(&self) -> Ident {
        format_ident!("__{}", &self.ident)
    }

    /// Vanilla associated type declaration (`type Foo: Bound;`) for
    /// emission inside the user-facing trait definition.
    pub fn trait_decl(&self) -> TokenStream {
        let attrs = attribute_tokens(&self.attrs);
        let ident = &self.ident;
        let bounds = &self.bounds;
        if bounds.is_empty() {
            quote! { #attrs type #ident; }
        } else {
            quote! { #attrs type #ident: #bounds; }
        }
    }

    /// Parses an associated type declaration within a remote trait body,
    /// given already-parsed outer attributes. Expects the input to be
    /// positioned at the `type` keyword.
    pub fn parse_with_attrs(input: ParseStream, attrs: Vec<Attribute>) -> syn::Result<Self> {
        input.parse::<Token![type]>()?;
        let ident: Ident = input.parse()?;
        if input.peek(Token![<]) {
            return Err(input.error("generic associated types are not supported in remote traits"));
        }

        let mut bounds: Punctuated<TypeParamBound, Token![+]> = Punctuated::new();
        if input.peek(Token![:]) {
            input.parse::<Token![:]>()?;
            loop {
                bounds.push_value(input.parse()?);
                if !input.peek(Token![+]) {
                    break;
                }
                bounds.push_punct(input.parse()?);
            }
        }

        if input.peek(Token![=]) {
            return Err(input.error("associated type defaults are not supported in remote traits"));
        }
        input.parse::<Token![;]>()?;

        Ok(Self { attrs, ident, bounds })
    }
}

/// Rewrites `Self::Foo` (and qualified form `<Self as Trait>::Foo`) to the
/// lifted ident (e.g. `__Foo`) for any of the given associated types.
pub fn remove_self_type(ty: &Type, assoc: &[AssocType]) -> Type {
    struct SelfAssocRewriter<'a> {
        pub assoc: &'a [AssocType],
    }

    impl<'a> VisitMut for SelfAssocRewriter<'a> {
        fn visit_type_mut(&mut self, t: &mut Type) {
            if let Type::Path(tp) = t {
                // Case: <Self as Trait>::Foo  -> __Foo
                if let Some(qs) = &tp.qself
                    && let Type::Path(self_tp) = &*qs.ty
                    && self_tp.path.is_ident("Self")
                    && let Some(last) = tp.path.segments.last()
                    && let Some(a) = self.assoc.iter().find(|a| a.ident == last.ident)
                {
                    let lifted = a.lifted_ident();
                    *t = Type::Path(TypePath { qself: None, path: lifted.into() });
                    return;
                }

                // Case: Self::Foo -> __Foo
                if tp.qself.is_none() {
                    let segs = &tp.path.segments;
                    if segs.len() == 2
                        && segs[0].ident == "Self"
                        && let Some(a) = self.assoc.iter().find(|a| a.ident == segs[1].ident)
                    {
                        let lifted = a.lifted_ident();
                        *t = Type::Path(TypePath { qself: None, path: lifted.into() });
                        return;
                    }
                }
            }

            syn::visit_mut::visit_type_mut(self, t);
        }
    }

    let mut ty = ty.clone();
    SelfAssocRewriter { assoc }.visit_type_mut(&mut ty);
    ty
}
