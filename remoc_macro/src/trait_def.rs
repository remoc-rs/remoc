//! Trait parsing and client and server generation.

use proc_macro2::TokenStream;
use quote::{TokenStreamExt, format_ident, quote};
use std::{collections::HashSet, str::FromStr};
use syn::{
    Attribute, GenericParam, Generics, Ident, Lifetime, LifetimeParam, Token, TypeParam, TypeParamBound,
    Visibility, WhereClause, braced,
    meta::ParseNestedMeta,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    token,
};

use crate::{
    assoc_type::AssocType,
    method::{SelfRef, TraitMethod},
    util::attribute_tokens,
};

/// Server variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ServerVariant {
    Value,
    Ref,
    RefMut,
    Shared,
    SharedMut,
}

struct InvalidServerVariant;

impl FromStr for ServerVariant {
    type Err = InvalidServerVariant;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "value" | "Value" => Ok(Self::Value),
            "ref" | "Ref" => Ok(Self::Ref),
            "ref_mut" | "RefMut" => Ok(Self::RefMut),
            "shared" | "Shared" => Ok(Self::Shared),
            "shared_mut" | "SharedMut" => Ok(Self::SharedMut),
            _ => Err(InvalidServerVariant),
        }
    }
}

/// Trait definition.
#[derive(Debug)]
pub struct TraitDef {
    /// Trait attributes.
    attrs: Vec<Attribute>,
    /// Trait visibility.
    vis: Visibility,
    /// Name.
    ident: Ident,
    /// Generics.
    /// Contains type parameter `Codec`.
    generics: Generics,
    /// Colon before supertraits.
    colon: Option<Token![:]>,
    /// Supertraits.
    supertraits: Punctuated<TypeParamBound, Token![+]>,
    /// Associated types declared in the trait.
    assoc_types: Vec<AssocType>,
    /// Methods.
    methods: Vec<TraitMethod>,
    /// Whether the `clone` attribute is present.
    clone: bool,
    /// Whether the `async_trait` attribute is present.
    async_trait: bool,
    /// Server variants to generate.
    server_variants: Option<HashSet<ServerVariant>>,
}

impl Parse for TraitDef {
    /// Parses a service trait.
    fn parse(input: ParseStream) -> syn::Result<Self> {
        // Parse trait definition.
        let attrs = input.call(Attribute::parse_outer)?;
        let vis: Visibility = input.parse()?;
        input.parse::<Token![trait]>()?;
        let ident: Ident = input.parse()?;

        // Parse generics.
        let mut generics = input.parse::<Generics>()?;
        if generics.params.iter().any(|p| matches!(p, GenericParam::Type(tp) if tp.ident == "Target")) {
            return Err(input.error("remote trait must not be generic over type parameter Target"));
        }
        if generics.lifetimes().count() > 0 {
            return Err(input.error("lifetimes are not allowed on remote traits"));
        }

        // Parse supertraits.
        let colon: Option<Token![:]> = input.parse()?;
        let mut supertraits = Punctuated::new();
        if colon.is_some() {
            loop {
                supertraits.push_value(input.parse()?);
                if input.peek(Token![where]) || input.peek(token::Brace) {
                    break;
                }
                supertraits.push_punct(input.parse()?);
            }
        }

        // Generics where clause.
        if let Some(where_clause) = input.parse::<Option<WhereClause>>()? {
            generics.make_where_clause().predicates.extend(where_clause.predicates);
        }

        // Extract content of trait definition.
        let content;
        braced!(content in input);

        // Parse associated type declarations and method definitions.
        let mut assoc_types: Vec<AssocType> = Vec::new();
        let mut methods: Vec<TraitMethod> = Vec::new();
        while !content.is_empty() {
            let attrs = content.call(Attribute::parse_outer)?;
            if content.peek(Token![type]) {
                assoc_types.push(AssocType::parse_with_attrs(&content, attrs)?);
            } else {
                let method = TraitMethod::parse(&ident, attrs, &content)?;
                methods.push(method);
            }
        }

        // The twin methods generated for a method must not collide with a method of
        // the trait.
        for method in &methods {
            let mut twins = vec![method.call_ident()];
            if method.pipelinable {
                twins.push(method.pipelined_ident());
            }

            for twin in twins {
                if methods.iter().any(|m| m.ident == twin) {
                    return Err(syn::Error::new(
                        method.ident.span(),
                        format!("`{twin}` is generated for this method and must not be declared"),
                    ));
                }
            }
        }

        Ok(Self {
            attrs,
            vis,
            ident,
            generics,
            colon,
            supertraits,
            assoc_types,
            methods,
            clone: false,
            async_trait: false,
            server_variants: None,
        })
    }
}

/// Arguments for the generics function.
#[derive(Debug, Clone, Copy)]
pub struct GenericsArgs {
    pub with_target: bool,
    pub with_codec: bool,
    pub with_codec_default: bool,
    pub with_lifetime: bool,
    pub with_send: bool,
    pub with_sync: bool,
    pub with_static: bool,
    /// If true, lift the trait's associated types into bare type
    /// parameters (with their bounds) on the produced generics.
    pub with_assoc_types: bool,
}

impl TraitDef {
    /// Parses and applies attributes specified by the procedural macro invocation.
    pub fn parse_meta(&mut self, meta: ParseNestedMeta) -> syn::Result<()> {
        if meta.path.is_ident("clone") {
            if self.is_taking_value() {
                return Err(meta.error("the client cannot be clonable if a method takes self by value"));
            }
            self.clone = true;
            Ok(())
        } else if meta.path.is_ident("async_trait") {
            self.async_trait = true;
            Ok(())
        } else if meta.path.is_ident("server") || meta.path.is_ident("Server") {
            let content;
            syn::parenthesized!(content in meta.input);

            let variants = self.server_variants.get_or_insert_default();
            while !content.is_empty() {
                let variant: Ident = content.parse()?;
                let variant = variant.to_string().parse::<ServerVariant>().map_err(|_| {
                    content.error("supported server variants: Value, Ref, RefMut, Shared, SharedMut")
                })?;
                variants.insert(variant);

                if content.peek(Token![,]) {
                    content.parse::<Token![,]>()?;
                } else {
                    break;
                }
            }

            Ok(())
        } else {
            Err(meta.error("unknown attribute"))
        }
    }

    /// True, if any trait method takes self by value.
    fn is_taking_value(&self) -> bool {
        self.methods.iter().any(|m| m.self_ref == SelfRef::Value)
    }

    /// True, if any trait method takes self by reference.
    fn is_taking_ref(&self) -> bool {
        self.methods.iter().any(|m| m.self_ref == SelfRef::Ref)
    }

    /// True, if any trait method takes self by mutable reference.
    fn is_taking_ref_mut(&self) -> bool {
        self.methods.iter().any(|m| m.self_ref == SelfRef::RefMut)
    }

    /// Identifier of the client type.
    fn client_ident(&self) -> Ident {
        format_ident!("{}Client", &self.ident)
    }

    /// Identifier of the request receiver type.
    fn req_receiver_ident(&self) -> Ident {
        format_ident!("{}ReqReceiver", &self.ident)
    }

    /// The `new` and `from_req_receiver` constructors of a server type.
    ///
    /// `target_ty` is how the server type holds its target object and `extra_fields`
    /// initializes the fields a server variant has in addition to the common ones.
    fn server_ctors(target_ty: TokenStream, extra_fields: TokenStream) -> TokenStream {
        quote! {
            fn with_request_buffer(target: #target_ty, request_buffer: usize) -> (Self, Self::Client) {
                let (req_tx, req_rx) = ::remoc::rch::mpsc::with_local_buffer(request_buffer);
                (
                    Self {
                        target,
                        req_rx,
                        monitor: ::std::boxed::Box::new(::remoc::rtc::DefaultMonitor),
                        #extra_fields
                    },
                    Self::Client::from_req_tx(req_tx),
                )
            }

            fn from_req_receiver(target: #target_ty, req_rx: Self::ReqReceiver) -> Self {
                Self {
                    target,
                    req_rx: req_rx.req_rx,
                    monitor: ::remoc::rtc::req_receiver_monitor_as_server_monitor(req_rx.monitor),
                    #extra_fields
                }
            }
        }
    }

    /// The generic arguments corresponding to the parameters of `generics`.
    fn generics_to_args(generics: &Generics) -> TokenStream {
        let args: Vec<TokenStream> = generics
            .params
            .iter()
            .map(|p| match p {
                GenericParam::Lifetime(lt) => {
                    let lt = &lt.lifetime;
                    quote! { #lt }
                }
                GenericParam::Type(tp) => {
                    let id = &tp.ident;
                    quote! { #id }
                }
                GenericParam::Const(cp) => {
                    let id = &cp.ident;
                    quote! { #id }
                }
            })
            .collect();

        if args.is_empty() {
            quote! {}
        } else {
            quote! { < #(#args),* > }
        }
    }

    /// Generic parameters for the versioning macro, which expects const generic
    /// parameters to follow the type generic parameters, separated by a semicolon.
    fn versioned_generics(ty_generics: &Generics) -> TokenStream {
        let mut ty_params = Vec::new();
        let mut const_params = Vec::new();
        for p in &ty_generics.params {
            match p {
                GenericParam::Type(tp) => {
                    let id = &tp.ident;
                    ty_params.push(quote! { #id });
                }
                GenericParam::Const(cp) => {
                    let (id, ty) = (&cp.ident, &cp.ty);
                    const_params.push(quote! { const #id: #ty });
                }
                GenericParam::Lifetime(_) => (),
            }
        }
        let consts = if const_params.is_empty() {
            quote! {}
        } else {
            quote! { ; #( #const_params ),* }
        };
        quote! { < #( #ty_params ),* #consts > }
    }

    /// Vanilla trait definition, without remote-specific attributes.
    pub fn vanilla_trait(&self) -> TokenStream {
        let Self { vis, ident, attrs, colon, supertraits, generics, .. } = self;
        let where_clause = &generics.where_clause;
        let mut attrs = attribute_tokens(attrs);

        // Associated type declarations.
        let mut defs = quote! {};
        for a in &self.assoc_types {
            defs.append_all(a.trait_decl());
        }

        // Trait methods.
        for m in &self.methods {
            defs.append_all(m.trait_method(!self.async_trait));
            defs.append_all(m.call_trait_method(!self.async_trait));
            if m.pipelinable {
                defs.append_all(m.pipelined_trait_method(!self.async_trait));
            }
        }

        if self.async_trait {
            attrs.extend(quote! { #[::async_trait::async_trait] });
        }

        quote! {
            #attrs
            #vis trait #ident #generics #colon #supertraits #where_clause {
                #defs
            }
        }
    }

    /// Generics for request enum, client type, server type and server trait implementation.
    ///
    /// First return item is server type generics, including Target, Codec and possibly lifetime of target.
    /// Second return itm is server implementation generics, including where-clauses on Target and Codec.
    fn generics(&self, args: GenericsArgs) -> (Generics, Generics) {
        let ident = &self.ident;

        let trait_generics = self.generics.clone();

        let mut ty_generics = self.generics.clone();
        let idx = ty_generics
            .params
            .iter()
            .enumerate()
            .find_map(|(idx, p)| match p {
                GenericParam::Const(_) => Some(idx),
                _ => None,
            })
            .unwrap_or_else(|| ty_generics.params.len());
        if args.with_codec {
            let codec_param: TypeParam = syn::parse2(if args.with_codec_default {
                quote! { Codec = ::remoc::codec::Default }
            } else {
                quote! { Codec }
            })
            .unwrap();
            ty_generics.params.insert(idx, GenericParam::Type(codec_param));
        }
        if args.with_target {
            ty_generics.params.insert(idx, GenericParam::Type(format_ident!("Target").into()));
        }

        // Insert associated types as bare type parameters (with their bounds
        // attached as where predicates so that derived `serde` `bound`
        // attributes pick them up).
        if args.with_assoc_types {
            for assoc in &self.assoc_types {
                let lifted = assoc.lifted_ident();
                // Insert before Target/Codec block, after the trait's own params.
                let insert_at = ty_generics
                    .params
                    .iter()
                    .position(
                        |p| matches!(p, GenericParam::Type(tp) if tp.ident == "Target" || tp.ident == "Codec"),
                    )
                    .unwrap_or(ty_generics.params.len());
                let tp: TypeParam = syn::parse2(quote! { #lifted }).unwrap();
                ty_generics.params.insert(insert_at, GenericParam::Type(tp));
            }
            // Add bounds for the inserted assoc types as where predicates.
            for assoc in &self.assoc_types {
                if assoc.bounds.is_empty() {
                    continue;
                }
                let lifted = assoc.lifted_ident();
                let bounds = &assoc.bounds;
                let wc: WhereClause = syn::parse2(quote! { where #lifted: #bounds }).unwrap();
                ty_generics.make_where_clause().predicates.extend(wc.predicates);
            }
        }

        if args.with_lifetime {
            let target_lt: Lifetime = syn::parse2(quote! {'target}).unwrap();
            ty_generics.params.insert(0, GenericParam::Lifetime(LifetimeParam::new(target_lt)));
        }

        let mut impl_generics = ty_generics.clone();

        if args.with_codec {
            let wc: WhereClause = syn::parse2(quote! { where Codec: ::remoc::codec::Codec }).unwrap();
            impl_generics.make_where_clause().predicates.extend(wc.predicates);
        }

        if args.with_target {
            let wc: WhereClause = syn::parse2(quote! { where Target: #ident #trait_generics }).unwrap();
            impl_generics.make_where_clause().predicates.extend(wc.predicates.clone());
            // Server struct field types use `<Target as Trait>::Foo` projections
            // when the trait has associated types, so the struct definition
            // also needs the `Target: Trait` bound.
            if !self.assoc_types.is_empty() {
                ty_generics.make_where_clause().predicates.extend(wc.predicates);
            }
        }

        if args.with_send {
            let wc: WhereClause = syn::parse2(quote! { where Target: ::std::marker::Send }).unwrap();
            impl_generics.make_where_clause().predicates.extend(wc.predicates);
        }

        if args.with_sync {
            let wc: WhereClause = syn::parse2(quote! { where Target: ::std::marker::Sync }).unwrap();
            impl_generics.make_where_clause().predicates.extend(wc.predicates);
        }

        if args.with_static {
            let wc: WhereClause = syn::parse2(quote! { where Target: 'static }).unwrap();
            impl_generics.make_where_clause().predicates.extend(wc.predicates);
        }

        (ty_generics, impl_generics)
    }

    /// Token stream of the trait's own generic argument idents (for use
    /// in turbofish position), without any added Target/Codec/assoc types.
    fn trait_generic_arg_tokens(&self) -> Vec<TokenStream> {
        self.generics
            .params
            .iter()
            .filter_map(|p| match p {
                GenericParam::Type(tp) => {
                    let id = &tp.ident;
                    Some(quote! { #id })
                }
                GenericParam::Const(cp) => {
                    let id = &cp.ident;
                    Some(quote! { #id })
                }
                GenericParam::Lifetime(_) => None,
            })
            .collect()
    }

    /// Trait path with associated-type bindings, e.g.
    /// `Trait<T1, T2, Foo = Foo, Bar = Bar>`. Used in `Target: Trait<...>`
    /// bounds in dispatch functions where the request enum's lifted assoc-
    /// type generics must be unified with the trait's projections.
    fn trait_path_with_assoc_bindings(&self) -> TokenStream {
        let ident = &self.ident;
        let trait_args = self.trait_generic_arg_tokens();
        if trait_args.is_empty() && self.assoc_types.is_empty() {
            return quote! { #ident };
        }
        let mut parts: Vec<TokenStream> = Vec::new();
        for t in trait_args {
            parts.push(t);
        }
        for a in &self.assoc_types {
            let n = &a.ident;
            let l = a.lifted_ident();
            parts.push(quote! { #n = #l });
        }
        quote! { #ident < #(#parts),* > }
    }

    /// Argument list for a request enum / client when used in a
    /// non-server context (assoc types appear as bare idents).
    /// Emits `<T1, T2, Foo, Bar, Codec>` (or omits `Codec` if not requested).
    fn req_args_bare(&self, with_codec: bool) -> TokenStream {
        let trait_args = self.trait_generic_arg_tokens();
        let assoc_args: Vec<TokenStream> = self
            .assoc_types
            .iter()
            .map(|a| {
                let id = a.lifted_ident();
                quote! { #id }
            })
            .collect();
        let mut parts: Vec<TokenStream> = Vec::new();
        parts.extend(trait_args);
        parts.extend(assoc_args);
        if with_codec {
            parts.push(quote! { Codec });
        }
        if parts.is_empty() {
            return quote! {};
        }
        quote! { < #(#parts),* > }
    }

    /// Argument list for a request enum / client when used in a server
    /// context where `Target: Trait` is in scope. Associated types are
    /// substituted by their projection through `Target`.
    /// Emits `<T1, T2, <Target as Trait<...>>::Foo, <Target as Trait<...>>::Bar, Codec>`.
    fn req_args_projected(&self, with_codec: bool) -> TokenStream {
        let ident = &self.ident;
        let trait_args = self.trait_generic_arg_tokens();
        let trait_path_args: TokenStream = if trait_args.is_empty() {
            quote! {}
        } else {
            let parts = &trait_args;
            quote! { < #(#parts),* > }
        };
        let assoc_args: Vec<TokenStream> = self
            .assoc_types
            .iter()
            .map(|a| {
                let n = &a.ident;
                quote! { <Target as #ident #trait_path_args>::#n }
            })
            .collect();
        let mut parts: Vec<TokenStream> = Vec::new();
        parts.extend(trait_args);
        parts.extend(assoc_args);
        if with_codec {
            parts.push(quote! { Codec });
        }
        if parts.is_empty() {
            return quote! {};
        }
        quote! { < #(#parts),* > }
    }

    /// Identifier of request enums for by-value, by-reference and by-mutable-reference requests.
    fn request_enum_idents(&self) -> (Ident, Ident, Ident) {
        (
            format_ident!("{}ReqValue", &self.ident),
            format_ident!("{}ReqRef", &self.ident),
            format_ident!("{}ReqRefMut", &self.ident),
        )
    }

    /// Requests enums with dispatch functions.
    pub fn request_enums(&self) -> TokenStream {
        let Self { vis, ident, .. } = self;
        let trait_name = ident.to_string();
        let assoc = &self.assoc_types;

        let (ty_generics, impl_generics) = self.generics(GenericsArgs {
            with_target: false,
            with_codec: true,
            with_codec_default: false,
            with_lifetime: false,
            with_send: false,
            with_sync: false,
            with_static: false,
            with_assoc_types: true,
        });
        let ty_generics_where = &ty_generics.where_clause;
        let (impl_generics_impl, impl_generics_ty, impl_generics_where) = impl_generics.split_for_impl();
        let (req_enum_impl, req_enum_ty, req_enum_where) = ty_generics.split_for_impl();
        let (req_value, req_ref, req_ref_mut) = self.request_enum_idents();
        let req_all = format_ident!("{}Req", &self.ident);
        let ty_generics_list = &ty_generics.params;

        let (ty_generics_codec_default, _) = self.generics(GenericsArgs {
            with_target: false,
            with_codec: true,
            with_codec_default: true,
            with_lifetime: false,
            with_send: false,
            with_sync: false,
            with_static: false,
            with_assoc_types: true,
        });

        let impl_generics_where_pred = &impl_generics_where.unwrap().predicates;
        let impl_generics_where_str = quote! { #impl_generics_where_pred }.to_string();

        // Trait path used in dispatch fn `Target: Trait<...>` bounds.
        // Includes assoc-type bindings unifying lifted assoc generics with
        // the target's projections.
        let trait_path_dispatch = self.trait_path_with_assoc_bindings();

        let (mut value_entries, mut ref_entries, mut ref_mut_entries) = (quote! {}, quote! {}, quote! {});
        let (mut value_clauses, mut ref_clauses, mut ref_mut_clauses) = (quote! {}, quote! {}, quote! {});
        let (mut value_names, mut ref_names, mut ref_mut_names) = (quote! {}, quote! {}, quote! {});
        let (mut value_seqs, mut ref_seqs, mut ref_mut_seqs) = (quote! {}, quote! {}, quote! {});
        for md in &self.methods {
            match md.self_ref {
                SelfRef::Value => {
                    value_entries.append_all(md.request_enum_entry(assoc));
                    value_clauses.append_all(md.dispatch_discriminator());
                    value_names.append_all(md.method_name_clause());
                    value_seqs.append_all(md.sequential_clause());
                }
                SelfRef::Ref => {
                    ref_entries.append_all(md.request_enum_entry(assoc));
                    ref_clauses.append_all(md.dispatch_discriminator());
                    ref_names.append_all(md.method_name_clause());
                    ref_seqs.append_all(md.sequential_clause());
                }
                SelfRef::RefMut => {
                    ref_mut_entries.append_all(md.request_enum_entry(assoc));
                    ref_mut_clauses.append_all(md.dispatch_discriminator());
                    ref_mut_names.append_all(md.method_name_clause());
                    ref_mut_seqs.append_all(md.sequential_clause());
                }
            }
        }

        let phantom_clause = quote! {
            Self::__Phantom(_) => ::std::unreachable!("__Phantom variant is not a valid request"),
        };

        let req_doc = |self_ref: &str| format!("Request generated by calling a method{self_ref} on [`{ident}`].");
        let req_doc_all = req_doc("");
        let req_doc_value = req_doc(" taking self by value (`self`)");
        let req_doc_ref = req_doc(" taking self by reference (`&self`)");
        let req_doc_ref_mut = req_doc("taking self by mutable reference (`&mut self`)");
        let phantom_doc = "Ignore this variant.\n\nIt can never occur.";

        quote! {
            #[doc = #req_doc_all]
            #vis type #req_all #ty_generics_codec_default = ::remoc::rtc::Req<
                #req_value #ty_generics,
                #req_ref #ty_generics,
                #req_ref_mut #ty_generics,
            >;

            #[doc = #req_doc_value]
            #[derive(::remoc::rtc::Serialize, ::remoc::rtc::Deserialize)]
            #[serde(crate = "::remoc::_serde")]
            #[serde(bound(serialize = #impl_generics_where_str))]
            #[serde(bound(deserialize = #impl_generics_where_str))]
            #vis enum #req_value #ty_generics #ty_generics_where {
                #value_entries
                #[doc = #phantom_doc]
                #[serde(skip)]
                __Phantom (::std::marker::PhantomData<(#ty_generics_list)>)
            }

            impl #impl_generics_impl #req_value #impl_generics_ty #impl_generics_where {
                fn dispatch<Target>(
                    self,
                    __target: Target,
                    __err_tx: ::remoc::rtc::ResponseErrorSender,
                    mut __guard: ::std::boxed::Box<dyn ::remoc::rtc::DispatchGuard>,
                ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ()> + ::std::marker::Send>>
                where
                    Target: #trait_path_dispatch,
                    Target: ::std::marker::Send + 'static,
                {
                    use ::remoc::rtc::FutureExt;
                    match self {
                        #value_clauses
                        #phantom_clause
                    }
                }
            }

            impl #req_enum_impl ::remoc::rtc::ReqEnum for #req_value #req_enum_ty #req_enum_where {
                fn trait_name() -> &'static str {
                    #trait_name
                }

                fn method_name(&self) -> &'static str {
                    match self {
                        #value_names
                        #phantom_clause
                    }
                }

                fn sequential(&self) -> bool {
                    match self {
                        #value_seqs
                        #phantom_clause
                    }
                }
            }

            #[doc = #req_doc_ref]
            #[derive(::remoc::rtc::Serialize, ::remoc::rtc::Deserialize)]
            #[serde(crate = "::remoc::_serde")]
            #[serde(bound(serialize = #impl_generics_where_str))]
            #[serde(bound(deserialize = #impl_generics_where_str))]
            #vis enum #req_ref #ty_generics #ty_generics_where {
                #ref_entries
                #[doc = #phantom_doc]
                #[serde(skip)]
                __Phantom (::std::marker::PhantomData<(#ty_generics_list)>)
            }

            impl #impl_generics_impl #req_ref #impl_generics_ty #impl_generics_where {
                fn dispatch<'target, Target>(
                    self,
                    __target: &'target Target,
                    __err_tx: ::remoc::rtc::ResponseErrorSender,
                    mut __guard: ::std::boxed::Box<dyn ::remoc::rtc::DispatchGuard>,
                ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ()> + ::std::marker::Send + 'target>>
                where
                    Target: #trait_path_dispatch,
                    Target: ::std::marker::Sync,
                {
                    use ::remoc::rtc::FutureExt;
                    match self {
                        #ref_clauses
                        #phantom_clause
                    }
                }
            }

            impl #req_enum_impl ::remoc::rtc::ReqEnum for #req_ref #req_enum_ty #req_enum_where {
                fn trait_name() -> &'static str {
                    #trait_name
                }

                fn method_name(&self) -> &'static str {
                    match self {
                        #ref_names
                        #phantom_clause
                    }
                }

                fn sequential(&self) -> bool {
                    match self {
                        #ref_seqs
                        #phantom_clause
                    }
                }
            }

            #[doc = #req_doc_ref_mut]
            #[derive(::remoc::rtc::Serialize, ::remoc::rtc::Deserialize)]
            #[serde(crate = "::remoc::_serde")]
            #[serde(bound(serialize = #impl_generics_where_str))]
            #[serde(bound(deserialize = #impl_generics_where_str))]
            #vis enum #req_ref_mut #ty_generics #ty_generics_where {
                #ref_mut_entries
                #[doc = #phantom_doc]
                #[serde(skip)]
                __Phantom (::std::marker::PhantomData<(#ty_generics_list)>)
            }

            impl #impl_generics_impl #req_ref_mut #impl_generics_ty #impl_generics_where {
                fn dispatch<'target, Target>(
                    self,
                    __target: &'target mut Target,
                    __err_tx: ::remoc::rtc::ResponseErrorSender,
                    mut __guard: ::std::boxed::Box<dyn ::remoc::rtc::DispatchGuard>,
                ) -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ()> + ::std::marker::Send + 'target>>
                where
                    Target: #trait_path_dispatch,
                    Target: ::std::marker::Send,
                {
                    use ::remoc::rtc::FutureExt;
                    match self {
                        #ref_mut_clauses
                        #phantom_clause
                    }
                }
            }

            impl #req_enum_impl ::remoc::rtc::ReqEnum for #req_ref_mut #req_enum_ty #req_enum_where {
                fn trait_name() -> &'static str {
                    #trait_name
                }

                fn method_name(&self) -> &'static str {
                    match self {
                        #ref_mut_names
                        #phantom_clause
                    }
                }

                fn sequential(&self) -> bool {
                    match self {
                        #ref_mut_seqs
                        #phantom_clause
                    }
                }
            }
        }
    }

    /// Server struct and implementation taking target by value.
    fn server_value(&self) -> TokenStream {
        let Self { vis, ident, .. } = self;

        let need_send = self.is_taking_value() || self.is_taking_ref_mut();
        let need_sync = self.is_taking_ref();
        let need_static = self.is_taking_value();

        let req_generics = self.req_args_projected(true);
        let (ty_generics, impl_generics) = self.generics(GenericsArgs {
            with_target: true,
            with_codec: true,
            with_codec_default: true,
            with_lifetime: false,
            with_send: need_send,
            with_sync: need_sync,
            with_static: need_static,
            with_assoc_types: false,
        });
        let ty_generics_where = &ty_generics.where_clause;
        let (impl_generics_impl, impl_generics_ty, impl_generics_where) = impl_generics.split_for_impl();

        let (req_value, req_ref, req_ref_mut) = self.request_enum_idents();
        let req_params = quote! {
            #req_value #req_generics,
            #req_ref #req_generics,
            #req_ref_mut #req_generics,
        };

        let client = self.client_ident();
        let req_receiver = self.req_receiver_ident();
        let server = format_ident!("{}Server", &ident);
        let ctors = Self::server_ctors(quote! { Target }, quote! {});

        let doc = format!("Server for [{}] taking the target object by value.", ident);

        let dispatch_value = if self.is_taking_value() {
            quote! { req.dispatch(target, err_tx.clone(), guard).await; }
        } else {
            quote! {}
        };

        let dispatch_ref = if self.is_taking_ref() {
            quote! { req.dispatch(&target, err_tx.clone(), guard).await; }
        } else {
            quote! {}
        };

        let dispatch_ref_mut = if self.is_taking_ref_mut() {
            quote! { req.dispatch(&mut target, err_tx.clone(), guard).await; }
        } else {
            quote! {}
        };

        quote! {
            #[doc=#doc]
            #vis struct #server #ty_generics #ty_generics_where {
                target: Target,
                req_rx: ::remoc::rch::mpsc::Receiver<
                    ::remoc::rtc::Req<#req_params>,
                    Codec,
                >,
                monitor: ::std::boxed::Box<dyn ::remoc::rtc::ServerMonitor<#req_params>>,
            }

            impl #impl_generics_impl ::remoc::rtc::ServerBase for #server #impl_generics_ty #impl_generics_where {
                type Client = #client #req_generics;
                type ReqReceiver = #req_receiver #req_generics;
            }

            impl #impl_generics_impl ::remoc::rtc::MonitorableServer for #server #impl_generics_ty #impl_generics_where {
                type Value = #req_value #req_generics;
                type Ref = #req_ref #req_generics;
                type RefMut = #req_ref_mut #req_generics;

                fn set_monitor(&mut self, monitor: impl ::remoc::rtc::ServerMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static) {
                    self.monitor = ::std::boxed::Box::new(monitor);
                }
            }

            impl #impl_generics_impl ::remoc::rtc::Server <Target, Codec> for #server #impl_generics_ty #impl_generics_where {
                #ctors

                async fn serve(self) -> (::std::option::Option<Target>, ::std::result::Result<(), ::remoc::rtc::ServeError>) {
                    let Self { mut target, mut req_rx, mut monitor } = self;
                    let (err_tx, mut err_rx) = ::remoc::rtc::response_error_channel();

                    let target_opt = loop {
                        ::remoc::rtc::select! {
                            biased;
                            Some(err) = err_rx.recv() => return (Some(target), Err(err)),
                            req = req_rx.recv() => {
                                let mut guard = ::remoc::rtc::server_monitor_pre_dispatch!(monitor, req, target);
                                match req {
                                    Ok(Some(::remoc::rtc::Req::Value(req))) => {
                                        #dispatch_value
                                        break None;
                                    },
                                    Ok(Some(::remoc::rtc::Req::Ref(req))) => {
                                        #dispatch_ref
                                    },
                                    Ok(Some(::remoc::rtc::Req::RefMut(req))) => {
                                        #dispatch_ref_mut
                                    },
                                    Ok(None) => break Some(target),
                                    Err(err) if err.is_disconnected() => break Some(target),
                                    Err(err) => return (Some(target), Err(err.into())),
                                }
                            }
                        }
                    };

                    drop(err_tx);
                    let res = match err_rx.recv().await {
                        None => Ok(()),
                        Some(err) => Err(err),
                    };

                    (target_opt, res)
                }
            }
        }
    }

    /// Server struct and implementation taking target by reference.
    fn server_ref(&self) -> TokenStream {
        let Self { vis, ident, .. } = self;

        let need_sync = self.is_taking_ref();

        let req_generics = self.req_args_projected(true);
        let (ty_generics, impl_generics) = self.generics(GenericsArgs {
            with_target: true,
            with_codec: true,
            with_codec_default: true,
            with_lifetime: true,
            with_send: false,
            with_sync: need_sync,
            with_static: false,
            with_assoc_types: false,
        });
        let ty_generics_where = &ty_generics.where_clause;
        let (impl_generics_impl, impl_generics_ty, impl_generics_where) = impl_generics.split_for_impl();

        let (req_value, req_ref, req_ref_mut) = self.request_enum_idents();
        let req_params = quote! {
            #req_value #req_generics,
            #req_ref #req_generics,
            #req_ref_mut #req_generics,
        };

        let client = self.client_ident();
        let req_receiver = self.req_receiver_ident();
        let server = format_ident!("{}ServerRef", &ident);
        let ctors = Self::server_ctors(quote! { &'target Target }, quote! {});

        let doc = format!("Server for [{}] taking the target object by reference.", ident);

        let dispatch_ref = if self.is_taking_ref() {
            quote! { req.dispatch(target, err_tx.clone(), guard).await; }
        } else {
            quote! {}
        };

        quote! {
            #[doc=#doc]
            #vis struct #server #ty_generics #ty_generics_where {
                target: &'target Target,
                req_rx: ::remoc::rch::mpsc::Receiver<
                    ::remoc::rtc::Req<#req_params>,
                    Codec,
                >,
                monitor: ::std::boxed::Box<dyn ::remoc::rtc::ServerMonitor<#req_params>>,
            }

            impl #impl_generics_impl ::remoc::rtc::ServerBase for #server #impl_generics_ty #impl_generics_where
            {
                type Client = #client #req_generics;
                type ReqReceiver = #req_receiver #req_generics;
            }

            impl #impl_generics_impl ::remoc::rtc::MonitorableServer for #server #impl_generics_ty #impl_generics_where
            {
                type Value = #req_value #req_generics;
                type Ref = #req_ref #req_generics;
                type RefMut = #req_ref_mut #req_generics;

                fn set_monitor(&mut self, monitor: impl ::remoc::rtc::ServerMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static) {
                    self.monitor = ::std::boxed::Box::new(monitor);
                }
            }

            impl #impl_generics_impl ::remoc::rtc::ServerRef <'target, Target, Codec> for #server #impl_generics_ty #impl_generics_where
            {
                #ctors

                async fn serve(self) -> ::std::result::Result<(), ::remoc::rtc::ServeError> {
                    let Self { target, mut req_rx, mut monitor } = self;
                    let (err_tx, mut err_rx) = ::remoc::rtc::response_error_channel();

                    let ret = loop {
                        ::remoc::rtc::select! {
                            biased;
                            Some(err) = err_rx.recv() => return Err(err),
                            req = req_rx.recv() => {
                                let guard = ::remoc::rtc::server_monitor_pre_dispatch!(monitor, req);
                                match req {
                                    Ok(Some(::remoc::rtc::Req::Ref(req))) => {
                                        #dispatch_ref
                                    },
                                    Ok(Some(_)) => (),
                                    Ok(None) => break,
                                    Err(err) if err.is_disconnected() => break,
                                    Err(err) => return Err(err.into()),
                                }
                            }
                        }
                    };

                    drop(err_tx);
                    match err_rx.recv().await {
                        None => Ok(ret),
                        Some(err) => Err(err),
                    }
                }
            }
        }
    }

    /// Server struct and implementation taking target by mutable reference.
    fn server_ref_mut(&self) -> TokenStream {
        let Self { vis, ident, .. } = self;

        let need_send = self.is_taking_value() || self.is_taking_ref_mut();
        let need_sync = self.is_taking_ref();

        let req_generics = self.req_args_projected(true);
        let (ty_generics, impl_generics) = self.generics(GenericsArgs {
            with_target: true,
            with_codec: true,
            with_codec_default: true,
            with_lifetime: true,
            with_send: need_send,
            with_sync: need_sync,
            with_static: false,
            with_assoc_types: false,
        });
        let ty_generics_where = &ty_generics.where_clause;
        let (impl_generics_impl, impl_generics_ty, impl_generics_where) = impl_generics.split_for_impl();

        let (req_value, req_ref, req_ref_mut) = self.request_enum_idents();
        let req_params = quote! {
            #req_value #req_generics,
            #req_ref #req_generics,
            #req_ref_mut #req_generics,
        };

        let client = self.client_ident();
        let req_receiver = self.req_receiver_ident();
        let server = format_ident!("{}ServerRefMut", &ident);
        let ctors = Self::server_ctors(quote! { &'target mut Target }, quote! {});

        let doc = format!("Server for [{}] taking the target object by mutable reference.", ident);

        let dispatch_ref = if self.is_taking_ref() {
            quote! { req.dispatch(target, err_tx.clone(), guard).await; }
        } else {
            quote! {}
        };

        let dispatch_ref_mut = if self.is_taking_ref_mut() {
            quote! { req.dispatch(target, err_tx.clone(), guard).await; }
        } else {
            quote! {}
        };

        quote! {
            #[doc=#doc]
            #vis struct #server #ty_generics #ty_generics_where {
                target: &'target mut Target,
                req_rx: ::remoc::rch::mpsc::Receiver<
                    ::remoc::rtc::Req<#req_params>,
                    Codec,
                >,
                monitor: ::std::boxed::Box<dyn ::remoc::rtc::ServerMonitor<#req_params>>,
            }

            impl #impl_generics_impl ::remoc::rtc::ServerBase for #server #impl_generics_ty #impl_generics_where
            {
                type Client = #client #req_generics;
                type ReqReceiver = #req_receiver #req_generics;
            }

            impl #impl_generics_impl ::remoc::rtc::MonitorableServer for #server #impl_generics_ty #impl_generics_where
            {
                type Value = #req_value #req_generics;
                type Ref = #req_ref #req_generics;
                type RefMut = #req_ref_mut #req_generics;

                fn set_monitor(&mut self, monitor: impl ::remoc::rtc::ServerMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static) {
                    self.monitor = ::std::boxed::Box::new(monitor);
                }
            }

            impl #impl_generics_impl ::remoc::rtc::ServerRefMut <'target, Target, Codec> for #server #impl_generics_ty #impl_generics_where
            {
                #ctors

                async fn serve(self) -> ::std::result::Result<(), ::remoc::rtc::ServeError> {
                    let Self { target, mut req_rx, mut monitor } = self;
                    let (err_tx, mut err_rx) = ::remoc::rtc::response_error_channel();

                    let ret = loop {
                        ::remoc::rtc::select! {
                            biased;
                            Some(err) = err_rx.recv() => return Err(err),
                            req = req_rx.recv() => {
                                let guard = ::remoc::rtc::server_monitor_pre_dispatch!(monitor, req);
                                match req {
                                    Ok(Some(::remoc::rtc::Req::Ref(req))) => {
                                        #dispatch_ref
                                    },
                                    Ok(Some(::remoc::rtc::Req::RefMut(req))) => {
                                        #dispatch_ref_mut
                                    },
                                    Ok(Some(_)) => (),
                                    Ok(None) => break,
                                    Err(err) if err.is_disconnected() => break,
                                    Err(err) => return Err(err.into()),
                                }
                            }
                        }
                    };

                    drop(err_tx);
                    match err_rx.recv().await {
                        None => Ok(ret),
                        Some(err) => Err(err),
                    }
                }
            }
        }
    }

    /// Server struct and implementation taking target by shared reference.
    fn server_shared(&self) -> TokenStream {
        let Self { vis, ident, .. } = self;

        let req_generics = self.req_args_projected(true);
        let (ty_generics, impl_generics) = self.generics(GenericsArgs {
            with_target: true,
            with_codec: true,
            with_codec_default: true,
            with_lifetime: false,
            with_send: true,
            with_sync: true,
            with_static: true,
            with_assoc_types: false,
        });
        let ty_generics_where = &ty_generics.where_clause;
        let (impl_generics_impl, impl_generics_ty, impl_generics_where) = impl_generics.split_for_impl();

        let (req_value, req_ref, req_ref_mut) = self.request_enum_idents();
        let req_params = quote! {
            #req_value #req_generics,
            #req_ref #req_generics,
            #req_ref_mut #req_generics,
        };

        let client = self.client_ident();
        let req_receiver = self.req_receiver_ident();
        let server = format_ident!("{}ServerShared", &ident);
        let ctors = Self::server_ctors(
            quote! { ::std::sync::Arc<Target> },
            quote! { parallelism: ::remoc::rtc::DEFAULT_PARALLELISM, },
        );

        let doc = format!("Server for [{}] taking the target object by shared reference.", ident);

        let dispatch_ref = if self.is_taking_ref() {
            quote! { req.dispatch(&*target, err_tx, guard).await; }
        } else {
            quote! {}
        };

        quote! {
            #[doc=#doc]
            #vis struct #server #ty_generics #ty_generics_where {
                target: ::std::sync::Arc<Target>,
                req_rx: ::remoc::rch::mpsc::Receiver<
                    ::remoc::rtc::Req<#req_params>,
                    Codec,
                >,
                monitor: ::std::boxed::Box<dyn ::remoc::rtc::ServerMonitor<#req_params>>,
                parallelism: usize,
            }

            impl #impl_generics_impl ::remoc::rtc::ServerBase for #server #impl_generics_ty #impl_generics_where
            {
                type Client = #client #req_generics;
                type ReqReceiver = #req_receiver #req_generics;
            }

            impl #impl_generics_impl ::remoc::rtc::MonitorableServer for #server #impl_generics_ty #impl_generics_where
            {
                type Value = #req_value #req_generics;
                type Ref = #req_ref #req_generics;
                type RefMut = #req_ref_mut #req_generics;

                fn set_monitor(&mut self, monitor: impl ::remoc::rtc::ServerMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static) {
                    self.monitor = ::std::boxed::Box::new(monitor);
                }
            }

            impl #impl_generics_impl ::remoc::rtc::ServerShared <Target, Codec> for #server #impl_generics_ty #impl_generics_where
            {
                #ctors

                fn parallelism(&self) -> usize {
                    self.parallelism
                }

                fn set_parallelism(&mut self, parallelism: usize) {
                    self.parallelism = parallelism;
                }

                async fn serve(self) -> ::std::result::Result<(), ::remoc::rtc::ServeError> {
                    let Self { target, mut req_rx, mut monitor, parallelism } = self;
                    let (err_tx, mut err_rx) = ::remoc::rtc::response_error_channel();
                    let semaphore = ::remoc::rtc::dispatch_semaphore(parallelism);

                    let ret = loop {
                        ::remoc::rtc::select! {
                            biased;
                            Some(err) = err_rx.recv() => return Err(err),
                            req = req_rx.recv() => {
                                let guard = ::remoc::rtc::server_monitor_pre_dispatch!(monitor, req);
                                match req {
                                    Ok(Some(::remoc::rtc::Req::Ref(req))) => {
                                        let err_tx = err_tx.clone();
                                        if parallelism > 0 && !::remoc::rtc::ReqEnum::sequential(&req) {
                                            use ::remoc::rtc::Instrument;
                                            let permit = ::remoc::rtc::acquire_dispatch_permit(&semaphore).await;
                                            let target = target.clone();
                                            ::remoc::rtc::spawn(async move {
                                                #dispatch_ref
                                                ::std::mem::drop(permit);
                                            }.in_current_span());
                                        } else {
                                            #dispatch_ref
                                        }
                                    },
                                    Ok(Some(_)) => (),
                                    Ok(None) => break,
                                    Err(err) if err.is_disconnected() => break,
                                    Err(err) => return Err(err.into()),
                                }
                            }
                        }
                    };

                    drop(err_tx);
                    match err_rx.recv().await {
                        None => Ok(ret),
                        Some(err) => Err(err),
                    }
                }
            }
        }
    }

    /// Server struct and implementation taking target by shared mutable reference.
    fn server_shared_mut(&self) -> TokenStream {
        let Self { vis, ident, .. } = self;

        let req_generics = self.req_args_projected(true);
        let (ty_generics, impl_generics) = self.generics(GenericsArgs {
            with_target: true,
            with_codec: true,
            with_codec_default: true,
            with_lifetime: false,
            with_send: true,
            with_sync: true,
            with_static: true,
            with_assoc_types: false,
        });
        let ty_generics_where = &ty_generics.where_clause;
        let (impl_generics_impl, impl_generics_ty, impl_generics_where) = impl_generics.split_for_impl();

        let (req_value, req_ref, req_ref_mut) = self.request_enum_idents();
        let req_params = quote! {
            #req_value #req_generics,
            #req_ref #req_generics,
            #req_ref_mut #req_generics,
        };

        let client = self.client_ident();
        let req_receiver = self.req_receiver_ident();
        let server = format_ident!("{}ServerSharedMut", &ident);
        let ctors = Self::server_ctors(
            quote! { ::std::sync::Arc<::remoc::rtc::LocalRwLock<Target>> },
            quote! { parallelism: ::remoc::rtc::DEFAULT_PARALLELISM, },
        );

        let doc = format!("Server for [{}] taking the target object by shared mutable reference.", ident);

        let dispatch_ref = if self.is_taking_ref() {
            quote! { req.dispatch(&*target, err_tx, guard).await; }
        } else {
            quote! {}
        };

        let dispatch_ref_mut = if self.is_taking_ref_mut() {
            quote! { req.dispatch(&mut *target, err_tx.clone(), guard).await; }
        } else {
            quote! {}
        };

        quote! {
            #[doc=#doc]
            #vis struct #server #ty_generics #ty_generics_where {
                target: ::std::sync::Arc<::remoc::rtc::LocalRwLock<Target>>,
                req_rx: ::remoc::rch::mpsc::Receiver<
                    ::remoc::rtc::Req<#req_params>,
                    Codec,
                >,
                monitor: ::std::boxed::Box<dyn ::remoc::rtc::ServerMonitor<#req_params>>,
                parallelism: usize,
            }

            impl #impl_generics_impl ::remoc::rtc::ServerBase for #server #impl_generics_ty #impl_generics_where
            {
                type Client = #client #req_generics;
                type ReqReceiver = #req_receiver #req_generics;
            }

            impl #impl_generics_impl ::remoc::rtc::MonitorableServer for #server #impl_generics_ty #impl_generics_where
            {
                type Value = #req_value #req_generics;
                type Ref = #req_ref #req_generics;
                type RefMut = #req_ref_mut #req_generics;

                fn set_monitor(&mut self, monitor: impl ::remoc::rtc::ServerMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static) {
                    self.monitor = ::std::boxed::Box::new(monitor);
                }
            }

            impl #impl_generics_impl ::remoc::rtc::ServerSharedMut <Target, Codec> for #server #impl_generics_ty #impl_generics_where
            {
                #ctors

                fn parallelism(&self) -> usize {
                    self.parallelism
                }

                fn set_parallelism(&mut self, parallelism: usize) {
                    self.parallelism = parallelism;
                }

                async fn serve(self) -> ::std::result::Result<(), ::remoc::rtc::ServeError> {
                    let Self { target, mut req_rx, mut monitor, parallelism } = self;
                    let (err_tx, mut err_rx) = ::remoc::rtc::response_error_channel();
                    let semaphore = ::remoc::rtc::dispatch_semaphore(parallelism);

                    let ret = loop {
                        ::remoc::rtc::select! {
                            biased;
                            Some(err) = err_rx.recv() => return Err(err),
                            req = req_rx.recv() => {
                                let guard = ::remoc::rtc::server_monitor_pre_dispatch!(monitor, req);
                                match req {
                                    Ok(Some(::remoc::rtc::Req::Ref(req))) => {
                                        let err_tx = err_tx.clone();
                                        if parallelism > 0 && !::remoc::rtc::ReqEnum::sequential(&req) {
                                            use ::remoc::rtc::Instrument;
                                            let permit = ::remoc::rtc::acquire_dispatch_permit(&semaphore).await;
                                            let target = target.clone().read_owned().await;
                                            ::remoc::rtc::spawn(async move {
                                                #dispatch_ref
                                                ::std::mem::drop(permit);
                                            }.in_current_span());
                                        } else {
                                            let target = target.read().await;
                                            #dispatch_ref
                                        }
                                    },
                                    Ok(Some(::remoc::rtc::Req::RefMut(req))) => {
                                        let mut target = target.write().await;
                                        #dispatch_ref_mut
                                    },
                                    Ok(Some(_)) => (),
                                    Ok(None) => break,
                                    Err(err) if err.is_disconnected() => break,
                                    Err(err) => return Err(err.into()),
                                }
                            }
                        }
                    };

                    drop(err_tx);
                    match err_rx.recv().await {
                        None => Ok(ret),
                        Some(err) => Err(err),
                    }
                }
            }
        }
    }

    /// Request receiver struct and implementation.
    fn req_receiver(&self) -> TokenStream {
        let Self { vis, ident, .. } = self;

        let req_generics = self.req_args_bare(true);
        let (ty_generics, impl_generics) = self.generics(GenericsArgs {
            with_target: false,
            with_codec: true,
            with_codec_default: true,
            with_lifetime: false,
            with_send: false,
            with_sync: false,
            with_static: false,
            with_assoc_types: true,
        });
        let ty_generics_where = &ty_generics.where_clause;
        let (impl_generics_impl, impl_generics_ty, impl_generics_where) = impl_generics.split_for_impl();
        let (req_value, req_ref, req_ref_mut) = self.request_enum_idents();

        let client = self.client_ident();
        let server = self.req_receiver_ident();

        let req_params = quote! {
            #req_value #req_generics,
            #req_ref #req_generics,
            #req_ref_mut #req_generics,
        };

        let doc = format!("Request receiver for [{}].\n\nCan be sent to a remote endpoint.", ident);

        let versioned_generics = Self::versioned_generics(&ty_generics);
        let impl_generics_where_pred = &impl_generics_where.unwrap().predicates;

        quote! {
            #[doc=#doc]
            #vis struct #server #ty_generics #ty_generics_where {
                req_rx: ::remoc::rch::mpsc::Receiver<
                    ::remoc::rtc::Req<#req_params>,
                    Codec,
                >,
                monitor: ::std::boxed::Box<dyn ::remoc::rtc::ReqReceiverMonitor<#req_params>>,
            }

            ::remoc::versioned::compact::impl_struct! {
                #server #versioned_generics,
                fields {
                    req_rx: ::remoc::rch::mpsc::Receiver<
                        ::remoc::rtc::Req<#req_params>,
                        Codec,
                    > => "_0",
                }
                default {
                    monitor = ::remoc::rtc::default_req_receiver_monitor(),
                }
                where #impl_generics_where_pred
            }

            impl #impl_generics_impl ::remoc::rtc::ServerBase for #server #impl_generics_ty #impl_generics_where
            {
                type Client = #client #req_generics;
                type ReqReceiver = Self;
            }

            impl #impl_generics_impl ::remoc::rtc::MonitorableReqReceiver for #server #impl_generics_ty #impl_generics_where
            {
                type Value = #req_value #req_generics;
                type Ref = #req_ref #req_generics;
                type RefMut = #req_ref_mut #req_generics;

                fn set_monitor(&mut self, monitor: impl ::remoc::rtc::ReqReceiverMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static) {
                    self.monitor = ::std::boxed::Box::new(monitor);
                }
            }

            impl #impl_generics_impl ::remoc::rtc::ReqReceiver <Codec> for #server #impl_generics_ty #impl_generics_where
            {
                type Value = #req_value #req_generics;
                type Ref = #req_ref #req_generics;
                type RefMut = #req_ref_mut #req_generics;

                fn with_request_buffer(request_buffer: usize) -> (Self, Self::Client) {
                    let (req_tx, req_rx) = ::remoc::rch::mpsc::with_local_buffer(request_buffer);
                    (
                        Self {
                            req_rx,
                            monitor: ::std::boxed::Box::new(::remoc::rtc::DefaultMonitor),
                        },
                        Self::Client::from_req_tx(req_tx),
                    )
                }

                async fn recv(&mut self) -> ::std::result::Result<::std::option::Option<
                    ::remoc::rtc::Req<Self::Value, Self::Ref, Self::RefMut>
                >, ::remoc::rch::mpsc::RecvError> {
                    loop {
                        let req = self.req_rx.recv().await;
                        ::remoc::rtc::req_receiver_monitor_pre_recv!(self.monitor, req);
                        break req
                    }
                }

                fn close(&mut self) {
                    self.req_rx.close()
                }

                async fn forward(mut self, client: Self::Client) -> ::std::result::Result<
                    ::std::option::Option<Self::Client>, ::remoc::rtc::ServeError
                > {
                    loop {
                        let permit = match client.req_tx.reserve().await {
                            ::std::result::Result::Ok(permit) => permit,
                            ::std::result::Result::Err(err) if err.is_closed() => {
                                break ::std::result::Result::Ok(::std::option::Option::Some(client))
                            }
                            ::std::result::Result::Err(err) => break ::std::result::Result::Err(err.into()),
                        };

                        let req = ::remoc::rtc::select! {
                            biased;
                            () = client.req_tx.closed() => continue,
                            req = self.recv() => req,
                        };

                        let req = match req {
                            ::std::result::Result::Ok(::std::option::Option::Some(req)) => req,
                            ::std::result::Result::Ok(::std::option::Option::None) => {
                                break ::std::result::Result::Ok(::std::option::Option::Some(client))
                            }
                            ::std::result::Result::Err(err) if err.is_disconnected() => {
                                break ::std::result::Result::Ok(::std::option::Option::Some(client))
                            }
                            ::std::result::Result::Err(err) => break ::std::result::Result::Err(err.into()),
                        };

                        match client.monitor.pre_call(&req).await {
                            ::remoc::rtc::CallDecision::Pass => (),
                            // The response is not delivered through the client, thus the guard is released immediately.
                            ::remoc::rtc::CallDecision::Guard(_guard) => (),
                            ::remoc::rtc::CallDecision::Drop => continue,
                        }

                        let consumed = ::std::matches!(req, ::remoc::rtc::Req::Value(_));

                        permit.send(req);

                        if consumed {
                            break ::std::result::Result::Ok(::std::option::Option::None);
                        }
                    }
                }
            }
        }
    }

    /// The server variants that are generated for this trait.
    ///
    /// This takes the variants requested by the `server(...)` argument and the
    /// methods of the trait into account.
    fn generated_server_variants(&self) -> Result<HashSet<ServerVariant>, &'static str> {
        let enabled = |variant: ServerVariant| match &self.server_variants {
            Some(variants) => variants.contains(&variant),
            None => false,
        };
        let enabled_or_auto = |variant: ServerVariant| match &self.server_variants {
            Some(variants) => variants.contains(&variant),
            None => true,
        };

        let mut variants = HashSet::new();

        // Always generate server taking value.
        if enabled_or_auto(ServerVariant::Value) {
            variants.insert(ServerVariant::Value);
        }

        // Generate servers taking (mutable, shared) references, if possible.
        if !self.is_taking_value() {
            if enabled_or_auto(ServerVariant::RefMut) {
                variants.insert(ServerVariant::RefMut);
            }
            if enabled_or_auto(ServerVariant::SharedMut) {
                variants.insert(ServerVariant::SharedMut);
            }

            if !self.is_taking_ref_mut() {
                if enabled_or_auto(ServerVariant::Ref) {
                    variants.insert(ServerVariant::Ref);
                }
                if enabled_or_auto(ServerVariant::Shared) {
                    variants.insert(ServerVariant::Shared);
                }
            } else {
                if enabled(ServerVariant::Ref) {
                    return Err("cannot generate ServerRef for trait containing methods that take '&mut self'");
                }
                if enabled(ServerVariant::Shared) {
                    return Err(
                        "cannot generate ServerShared for trait containing methods that take '&mut self'",
                    );
                }
            }
        } else {
            if enabled(ServerVariant::RefMut) {
                return Err("cannot generate ServerRefMut for trait containing methods that take 'self'");
            }
            if enabled(ServerVariant::SharedMut) {
                return Err("cannot generate ServerSharedMut for trait containing methods that take 'self'");
            }
        }

        Ok(variants)
    }

    /// Conversion of the request receiver into a single server type.
    ///
    /// `method` is the name of the generated function, `server` the server type it
    /// returns and `server_trait` the path of the server trait, including its generic
    /// arguments. `target_ty` is how that server holds its target object.
    #[allow(clippy::too_many_arguments)]
    fn req_receiver_into_server(
        &self, method: &str, server: Ident, server_trait: TokenStream, target_ty: TokenStream,
        with_lifetime: bool, with_send: bool, with_sync: bool, with_static: bool,
    ) -> TokenStream {
        let vis = &self.vis;
        let method = format_ident!("{method}");

        // Generic arguments of the server type, as seen from the request receiver.
        let (server_ty_generics, _) = self.generics(GenericsArgs {
            with_target: true,
            with_codec: true,
            with_codec_default: true,
            with_lifetime,
            with_send: false,
            with_sync: false,
            with_static: false,
            with_assoc_types: false,
        });
        let server_args = Self::generics_to_args(&server_ty_generics);

        let lifetime = if with_lifetime {
            quote! { 'target, }
        } else {
            quote! {}
        };

        // The target object must implement the trait, with its associated types
        // matching those of the request receiver.
        let target_bound = self.trait_path_with_assoc_bindings();
        let mut bounds = quote! { Target: #target_bound, };
        if with_send {
            bounds.append_all(quote! { Target: ::std::marker::Send, });
        }
        if with_sync {
            bounds.append_all(quote! { Target: ::std::marker::Sync, });
        }
        if with_static {
            bounds.append_all(quote! { Target: 'static, });
        }

        let doc = format!("Converts this into a [{server}], which dispatches the requests to `target`.");

        quote! {
            #[doc=#doc]
            ///
            /// Requests that are already queued in the request receiver are processed by
            /// the server.
            #vis fn #method <#lifetime Target> (self, target: #target_ty) -> #server #server_args
            where
                #bounds
            {
                <#server #server_args as ::remoc::rtc::#server_trait>::from_req_receiver(target, self)
            }
        }
    }

    /// Conversions of the request receiver into the generated server types.
    fn req_receiver_into_servers(&self, variants: &HashSet<ServerVariant>) -> TokenStream {
        let ident = &self.ident;
        let req_receiver = self.req_receiver_ident();

        let (_, impl_generics) = self.generics(GenericsArgs {
            with_target: false,
            with_codec: true,
            with_codec_default: true,
            with_lifetime: false,
            with_send: false,
            with_sync: false,
            with_static: false,
            with_assoc_types: true,
        });
        let (impl_generics_impl, impl_generics_ty, impl_generics_where) = impl_generics.split_for_impl();

        let need_send = self.is_taking_value() || self.is_taking_ref_mut();
        let need_sync = self.is_taking_ref();

        let mut methods = quote! {};

        if variants.contains(&ServerVariant::Value) {
            methods.append_all(self.req_receiver_into_server(
                "into_server",
                format_ident!("{ident}Server"),
                quote! { Server<Target, Codec> },
                quote! { Target },
                false,
                need_send,
                need_sync,
                self.is_taking_value(),
            ));
        }

        if variants.contains(&ServerVariant::Ref) {
            methods.append_all(self.req_receiver_into_server(
                "into_server_ref",
                format_ident!("{ident}ServerRef"),
                quote! { ServerRef<'target, Target, Codec> },
                quote! { &'target Target },
                true,
                false,
                need_sync,
                false,
            ));
        }

        if variants.contains(&ServerVariant::RefMut) {
            methods.append_all(self.req_receiver_into_server(
                "into_server_ref_mut",
                format_ident!("{ident}ServerRefMut"),
                quote! { ServerRefMut<'target, Target, Codec> },
                quote! { &'target mut Target },
                true,
                need_send,
                need_sync,
                false,
            ));
        }

        if variants.contains(&ServerVariant::Shared) {
            methods.append_all(self.req_receiver_into_server(
                "into_server_shared",
                format_ident!("{ident}ServerShared"),
                quote! { ServerShared<Target, Codec> },
                quote! { ::std::sync::Arc<Target> },
                false,
                true,
                true,
                true,
            ));
        }

        if variants.contains(&ServerVariant::SharedMut) {
            methods.append_all(self.req_receiver_into_server(
                "into_server_shared_mut",
                format_ident!("{ident}ServerSharedMut"),
                quote! { ServerSharedMut<Target, Codec> },
                quote! { ::std::sync::Arc<::remoc::rtc::LocalRwLock<Target>> },
                false,
                true,
                true,
                true,
            ));
        }

        if methods.is_empty() {
            return quote! {};
        }

        quote! {
            impl #impl_generics_impl #req_receiver #impl_generics_ty #impl_generics_where {
                #methods
            }
        }
    }

    /// Server types and implementations.
    pub fn servers(&self) -> Result<TokenStream, &'static str> {
        let variants = self.generated_server_variants()?;

        let mut servers = quote! {};

        if variants.contains(&ServerVariant::Value) {
            servers.append_all(self.server_value());
        }
        if variants.contains(&ServerVariant::RefMut) {
            servers.append_all(self.server_ref_mut());
        }
        if variants.contains(&ServerVariant::SharedMut) {
            servers.append_all(self.server_shared_mut());
        }
        if variants.contains(&ServerVariant::Ref) {
            servers.append_all(self.server_ref());
        }
        if variants.contains(&ServerVariant::Shared) {
            servers.append_all(self.server_shared());
        }

        // The request receiver is always generated, since every server variant can be constructed from it.
        servers.append_all(self.req_receiver());
        servers.append_all(self.req_receiver_into_servers(&variants));

        Ok(servers)
    }

    /// The client proxy.
    pub fn client(&self) -> TokenStream {
        let Self { vis, ident, attrs, generics, .. } = self;
        let attrs = attribute_tokens(attrs);
        let client_ident = self.client_ident();
        let client_ident_str = client_ident.to_string();
        let req_receiver = self.req_receiver_ident();

        let (ty_generics, impl_generics) = self.generics(GenericsArgs {
            with_target: false,
            with_codec: true,
            with_codec_default: true,
            with_lifetime: false,
            with_send: false,
            with_sync: false,
            with_static: false,
            with_assoc_types: true,
        });
        let ty_generics_where_ty = &ty_generics.where_clause;
        let (ty_generics_impl, ty_generics_ty, ty_generics_where) = ty_generics.split_for_impl();
        let (impl_generics_impl, impl_generics_ty, impl_generics_where) = impl_generics.split_for_impl();

        let req_generics = self.req_args_bare(true);
        let (req_value, req_ref, req_ref_mut) = self.request_enum_idents();
        let req_params = quote! {
            #req_value #req_generics,
            #req_ref #req_generics,
            #req_ref_mut #req_generics,
        };

        let impl_generics_where_pred = &impl_generics_where.unwrap().predicates;

        let versioned_generics = Self::versioned_generics(&ty_generics);

        let assoc = &self.assoc_types;

        // Generate client method implementations.
        let mut methods = quote! {};
        for m in &self.methods {
            methods.append_all(m.client_method(&req_value, &req_ref, &req_ref_mut, assoc));
        }

        // Associated type items for the client's `impl Trait for Client`.
        let mut assoc_impl_items = quote! {};
        for a in &self.assoc_types {
            let n = &a.ident;
            let l = a.lifted_ident();
            assoc_impl_items.append_all(quote! { type #n = #l; });
        }

        let doc = format!("Remote client for [{}].\n\nCan be sent to a remote endpoint.", ident);

        // Allowing cloning if object is accessed by reference only.
        let clone = if (!self.is_taking_ref_mut() || self.clone) && !self.is_taking_value() {
            quote! {
                impl #impl_generics_impl Clone for #client_ident #impl_generics_ty #ty_generics_where {
                    fn clone(&self) -> Self {
                        Self {
                            req_tx: self.req_tx.clone(),
                            max_response_size: self.max_response_size,
                            sequential: self.sequential,
                            stop_on_error: self.stop_on_error,
                            drop_tx: self.drop_tx.clone(),
                            monitor: self.monitor.clone(),
                        }
                    }
                }
            }
        } else {
            quote! {}
        };

        let async_trait = if self.async_trait {
            quote! { #[::async_trait::async_trait] }
        } else {
            quote! {}
        };

        quote! {
            #[doc=#doc]
            #attrs
            #vis struct #client_ident #ty_generics #ty_generics_where_ty {
                req_tx: ::remoc::rch::mpsc::Sender<
                    ::remoc::rtc::Req<#req_value #req_generics, #req_ref #req_generics, #req_ref_mut #req_generics>,
                    Codec,
                >,
                max_response_size: u64,
                sequential: bool,
                stop_on_error: bool,
                drop_tx: ::remoc::rtc::local_broadcast::Sender<()>,
                monitor: ::std::sync::Arc<dyn ::remoc::rtc::ClientMonitor<#req_params>>,
            }

            ::remoc::versioned::compact::impl_struct! {
                #client_ident #versioned_generics,
                fields {
                    req_tx: ::remoc::rch::mpsc::Sender<
                        ::remoc::rtc::Req<#req_value #req_generics, #req_ref #req_generics, #req_ref_mut #req_generics>,
                        Codec,
                    > => "_0",
                    #[serde(default = "::remoc::rch::default_max_item_size")]
                    #[serde(skip_serializing_if = "::remoc::rch::is_default_max_item_size")]
                    max_response_size: u64 => "_1",
                    #[serde(default)]
                    #[serde(skip_serializing_if = "::remoc::codec::skip::if_default_ref")]
                    sequential: bool => "_2",
                    #[serde(default)]
                    #[serde(skip_serializing_if = "::remoc::codec::skip::if_default_ref")]
                    stop_on_error: bool => "_3",
                }
                default {
                    drop_tx = ::remoc::rtc::empty_client_drop_tx(),
                    monitor = ::remoc::rtc::default_client_monitor(),
                }
                where #impl_generics_where_pred
            }

            #clone

            impl #impl_generics_impl #client_ident #impl_generics_ty #impl_generics_where {
                /// Creates the response channel for a request, carrying the call
                /// options of this client.
                #[doc(hidden)]
                pub fn __responder<__R>(
                    &self,
                ) -> (
                    ::remoc::rtc::Responder<__R, Codec>,
                    ::remoc::rch::oneshot::Receiver<::remoc::rtc::TransportedResponse<__R>, Codec>,
                )
                where
                    __R: ::remoc::rtc::Response,
                    ::remoc::rtc::TransportedResponse<__R>: ::remoc::RemoteSend,
                {
                    let (responder, response_rx) = ::remoc::rtc::response_channel(::remoc::rtc::Client::max_response_size(self));
                    let responder = ::remoc::rtc::Responder::new(
                        responder, self.sequential, self.stop_on_error,
                    );
                    (responder, response_rx)
                }

                fn from_req_tx(req_tx: ::remoc::rch::mpsc::Sender<
                    ::remoc::rtc::Req<#req_value #req_generics, #req_ref #req_generics, #req_ref_mut #req_generics>,
                    Codec,
                >) -> Self
                {
                    Self {
                        req_tx,
                        max_response_size: ::remoc::rch::default_max_item_size(),
                        sequential: false,
                        stop_on_error: false,
                        drop_tx: ::remoc::rtc::empty_client_drop_tx(),
                        monitor: ::remoc::rtc::default_client_monitor(),
                    }
                }
            }

            impl #impl_generics_impl ::remoc::rtc::Client for #client_ident #impl_generics_ty #impl_generics_where {
                type ReqReceiver = #req_receiver #req_generics;

                fn with_request_buffer(request_buffer: usize) -> (Self, Self::ReqReceiver) {
                    let (req_rx, client) =
                        <#req_receiver #req_generics as ::remoc::rtc::ReqReceiver<Codec>>::with_request_buffer(
                            request_buffer,
                        );
                    (client, req_rx)
                }

                fn capacity(&self) -> usize {
                    self.req_tx.capacity()
                }

                fn closed(&self) -> ::remoc::rtc::Closed {
                    let req_tx = self.req_tx.clone();
                    let mut drop_rx = self.drop_tx.subscribe();
                    ::remoc::rtc::Closed::new(async move {
                        ::remoc::rtc::select! {
                            () = req_tx.closed() => (),
                            _ = drop_rx.recv() => (),
                        }
                    })
                }

                fn is_closed(&self) -> bool {
                    self.req_tx.is_closed()
                }

                fn max_request_size(&self) -> usize {
                    self.req_tx.max_item_size()
                }

                fn set_max_request_size(&mut self, max_request_size: usize) {
                    self.req_tx.set_max_item_size(max_request_size);
                }

                fn max_response_size(&self) -> usize {
                    ::std::convert::TryFrom::try_from(self.max_response_size).unwrap_or(::std::usize::MAX)
                }

                fn set_max_response_size(&mut self, max_response_size: usize) {
                    self.max_response_size =
                        ::std::convert::TryFrom::try_from(max_response_size).unwrap_or(u64::MAX)
                }

                fn sequential(&self) -> bool {
                    self.sequential
                }

                fn set_sequential(&mut self, sequential: bool) {
                    self.sequential = sequential
                }

                fn stop_on_error(&self) -> bool {
                    self.stop_on_error
                }

                fn set_stop_on_error(&mut self, stop_on_error: bool) {
                    self.stop_on_error = stop_on_error
                }
            }

            impl #impl_generics_impl ::remoc::rtc::MonitorableClient for #client_ident #impl_generics_ty #impl_generics_where {
                type Value = #req_value #req_generics;
                type Ref = #req_ref #req_generics;
                type RefMut = #req_ref_mut #req_generics;

                fn set_monitor(&mut self, monitor: impl ::remoc::rtc::ClientMonitor<Self::Value, Self::Ref, Self::RefMut> + 'static) {
                    self.monitor = ::std::sync::Arc::new(monitor);
                }
            }

            #async_trait
            impl #impl_generics_impl #ident #generics for #client_ident #impl_generics_ty #impl_generics_where {
                #assoc_impl_items
                #methods
            }

            impl #ty_generics_impl ::std::fmt::Debug for #client_ident #ty_generics_ty #ty_generics_where {
                fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                    write!(f, #client_ident_str)
                }
            }

            impl #ty_generics_impl ::std::ops::Drop for #client_ident #ty_generics_ty #ty_generics_where {
                fn drop(&mut self) {
                    // required for drop order
                }
            }
        }
    }
}
