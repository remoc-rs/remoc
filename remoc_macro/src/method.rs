//! Method parsing and generation.

use proc_macro2::{TokenStream, TokenTree};
use quote::{TokenStreamExt, format_ident, quote};
use syn::{
    Attribute, Block, FnArg, GenericArgument, Generics, Ident, LitStr, Meta, Pat, PatType, Path, PathArguments,
    ReceiverKind, ReturnType, Stmt, Token, Type, TypeParamBound, braced, parenthesized,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    spanned::Spanned,
    token::{self, Comma},
};

use crate::{
    assoc_type::{AssocType, remove_self_type},
    util::{attribute_tokens, to_pascal_case},
};

/// Self reference of method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfRef {
    /// self
    Value,
    /// &self
    Ref,
    /// &mut self
    RefMut,
}

/// The numerical name reserved by remoc for the reply channel field within
/// generated request enums.
const REPLY_TX_NAME: &str = "_59";

/// Whether the name is a numerical identifier that the Postbag codec encodes
/// using a single byte.
fn is_numerical_name(name: &str) -> bool {
    match name.strip_prefix('_').map(str::parse::<usize>) {
        Some(Ok(id)) => id < 60,
        _ => false,
    }
}

/// Skips over the value of a meta item, i.e. everything up to the next comma
/// at the current nesting level.
fn skip_meta_value(input: ParseStream) -> syn::Result<()> {
    input.step(|cursor| {
        let mut rest = *cursor;
        while let Some((tt, next)) = rest.token_tree() {
            match &tt {
                TokenTree::Punct(punct) if punct.as_char() == ',' => break,
                _ => rest = next,
            }
        }
        Ok(((), rest))
    })
}

/// Whether the attributes contain a serde rename to a numerical identifier
/// and validation that the name reserved by remoc is not used.
fn numerical_serde_rename(attrs: &[Attribute]) -> syn::Result<bool> {
    let mut numerical = false;

    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }

        attr.parse_nested_meta(|meta| {
            if !meta.path.is_ident("rename") {
                return skip_meta_value(meta.input);
            }

            let mut check = |value: ParseStream| -> syn::Result<()> {
                let name: LitStr = value.parse()?;

                // Reject the reserved name, since it would silently collide with
                // the reply channel field of the generated request enum.
                if name.value() == REPLY_TX_NAME {
                    return Err(syn::Error::new(
                        name.span(),
                        format!("the name `{REPLY_TX_NAME}` is reserved by remoc"),
                    ));
                }

                numerical |= is_numerical_name(&name.value());
                Ok(())
            };

            if meta.input.peek(Token![=]) {
                // #[serde(rename = "...")]
                check(meta.value()?)?;
            } else {
                // #[serde(rename(serialize = "...", deserialize = "..."))]
                meta.parse_nested_meta(|meta| check(meta.value()?))?;
            }

            Ok(())
        })?;
    }

    Ok(numerical)
}

/// A named argument.
#[derive(Debug)]
pub struct NamedArg {
    /// Attributes.
    pub attrs: Vec<Attribute>,
    /// Name.
    pub ident: Ident,
    /// Type.
    pub ty: Type,
}

impl NamedArg {
    /// Create a `NamedArg` from a `PatType`.
    ///
    /// Returns whether the argument is renamed to a numerical identifier by the user.
    fn extract(pat_type: &PatType) -> syn::Result<(Self, bool)> {
        let ident = if let Pat::Ident(pat_ident) = &*pat_type.pat {
            pat_ident.ident.clone()
        } else {
            return Err(syn::Error::new(pat_type.pat.span(), "expected identifier"));
        };
        let numerical_rename = numerical_serde_rename(&pat_type.attrs)?;
        Ok((Self { attrs: pat_type.attrs.clone(), ident, ty: (*pat_type.ty).clone() }, numerical_rename))
    }
}

/// A method in a trait.
#[derive(Debug)]
pub struct TraitMethod {
    /// Documentation attributes, applied to the trait method and the request enum variant.
    pub doc_attrs: Vec<Attribute>,
    /// Serde attributes, applied to the request enum variant.
    pub serde_attrs: Vec<Attribute>,
    /// Whether the request enum variant or any of its fields is renamed to a
    /// numerical identifier by the user.
    ///
    /// This indicates that the user opted into the compact serialized
    /// representation for this method, allowing remoc to also use a compact
    /// name for the reply channel field.
    pub numerical_rename: bool,
    /// Other attributes, applied to the trait method.
    pub attrs: Vec<Attribute>,
    /// Name.
    pub ident: Ident,
    /// Self reference of method.
    pub self_ref: SelfRef,
    /// Arguments.
    pub args: Vec<NamedArg>,
    /// Return type.
    pub ret_ty: Type,
    /// Trait bounds when return type is `impl Future + ...`
    pub bounds: Punctuated<TypeParamBound, Token![+]>,
    /// Whether method should be cancelled, if client sends hangup message.
    pub cancel: bool,
    /// Whether a twin method taking a request receiver should be generated.
    pub pipelinable: bool,
    /// Name of that twin method, if specified by the user.
    pub pipelined_name: Option<Ident>,
    /// Method body.
    pub body: Option<Vec<Stmt>>,
}

/// The output type of a `std::future::Future<Output = ...>` or equivalent.
fn future_output_type(path: &Path) -> Option<&Type> {
    let args = match (path.segments.get(0), path.segments.get(1), path.segments.get(2)) {
        (Some(p0), None, None) if p0.ident == "Future" => &p0.arguments,
        (Some(p0), Some(p1), Some(p2))
            if (p0.ident == "std" || p0.ident == "core") && p1.ident == "future" && p2.ident == "Future" =>
        {
            &p2.arguments
        }
        _ => return None,
    };

    let PathArguments::AngleBracketed(args) = args else { return None };
    for arg in &args.args {
        let GenericArgument::AssocType(ty) = arg else { continue };
        if ty.ident == "Output" {
            return Some(&ty.ty);
        }
    }

    None
}

/// Whether the path is `Send` or equivalent.
fn is_send(path: &Path) -> bool {
    match (path.segments.get(0), path.segments.get(1), path.segments.get(2)) {
        (Some(p0), None, None) if p0.ident == "Send" => true,
        (Some(p0), Some(p1), Some(p2))
            if (p0.ident == "std" || p0.ident == "core") && p1.ident == "marker" && p2.ident == "Send" =>
        {
            true
        }
        _ => false,
    }
}

impl Parse for TraitMethod {
    /// Parses a method within the service trait.
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        Self::parse_with_attrs(input, attrs)
    }
}

impl TraitMethod {
    /// Parses a method within the service trait, given already-parsed outer attributes.
    pub fn parse_with_attrs(input: ParseStream, mut attrs: Vec<Attribute>) -> syn::Result<Self> {
        // Parse method definition.
        let is_async = input.parse::<Option<Token![async]>>()?.is_some();
        input.parse::<Token![fn]>()?;
        let ident: Ident = input.parse()?;

        // Check for no_cancel and pipelinable attributes.
        let mut cancel = true;
        let mut pipelinable = false;
        let mut pipelined_name = None;
        let mut attr_err = None;
        attrs.retain(|attr| {
            let Some(name) = attr.path().get_ident() else { return true };

            if *name == "no_cancel" {
                cancel = false;
                return false;
            }

            if *name == "pipelinable" {
                pipelinable = true;
                match &attr.meta {
                    // #[pipelinable]
                    Meta::Path(_) => (),
                    // #[pipelinable(twin_method_name)]
                    Meta::List(_) => match attr.parse_args::<Ident>() {
                        Ok(name) => pipelined_name = Some(name),
                        Err(err) => attr_err = Some(err),
                    },
                    Meta::NameValue(_) => {
                        attr_err = Some(syn::Error::new_spanned(
                            attr,
                            "expected `#[pipelinable]` or `#[pipelinable(name)]`",
                        ))
                    }
                }
                return false;
            }

            true
        });
        if let Some(err) = attr_err {
            return Err(err);
        }

        // Split remaining attributes by how they are applied to the generated items.
        let mut doc_attrs = Vec::new();
        let mut serde_attrs = Vec::new();
        attrs.retain(|attr| {
            if attr.path().is_ident("doc") {
                doc_attrs.push(attr.clone());
                false
            } else if attr.path().is_ident("serde") {
                serde_attrs.push(attr.clone());
                false
            } else {
                true
            }
        });
        let mut numerical_rename = numerical_serde_rename(&serde_attrs)?;

        // Parse generics.
        let generics = input.parse::<Generics>()?;
        if generics.lt_token.is_some() {
            return Err(input.error("generics and lifetimes are not allowed on remote trait methods"));
        }

        // Parse arguments.
        let content;
        parenthesized!(content in input);
        let raw_args: Punctuated<FnArg, Comma> = content.parse_terminated(FnArg::parse, Token![,])?;

        // Extract receiver and arguments.
        let mut self_ref = None;
        let mut args = Vec::new();
        for arg in raw_args {
            match arg {
                // self, &self or &mut self receiver
                FnArg::Receiver(recv) => {
                    self_ref = Some(match recv.kind {
                        ReceiverKind::Reference(_, _, Some(_)) => SelfRef::RefMut,
                        ReceiverKind::Reference(_, _, None) => SelfRef::Ref,
                        ReceiverKind::Value => SelfRef::Value,
                        _ => {
                            return Err(
                                input.error("only methods taking self, &self and &mut self are supported")
                            );
                        }
                    });
                }
                // other argument
                FnArg::Typed(pat_type) => {
                    let (arg, arg_numerical_rename) = NamedArg::extract(&pat_type)?;
                    numerical_rename |= arg_numerical_rename;
                    args.push(arg);
                }
            }
        }
        let self_ref =
            self_ref.ok_or_else(|| input.error("associated functions are not allowed in remote traits"))?;

        // Parse return type.
        let ret: ReturnType = input.parse()?;
        let ret_ty = match ret {
            ReturnType::Type(_, ty) => {
                if is_async {
                    // async fn name() -> Result<_>
                    Some((*ty, true, Punctuated::new()))
                } else {
                    // fn name() -> impl Future<Output = Result<_>> + Send
                    match *ty {
                        Type::ImplTrait(impl_trait) => {
                            let mut others: Punctuated<TypeParamBound, Token![+]> = Punctuated::new();
                            let mut output = None;
                            let mut has_send = false;

                            for bound in impl_trait.bounds {
                                match bound {
                                    TypeParamBound::Trait(tb) if is_send(&tb.path) => has_send = true,
                                    TypeParamBound::Trait(tb) if future_output_type(&tb.path).is_some() => {
                                        output = future_output_type(&tb.path).cloned()
                                    }
                                    _ => others.push(bound),
                                }
                            }

                            output.map(|output| (output, has_send, others))
                        }
                        _ => None,
                    }
                }
            }
            ReturnType::Default => None,
        };
        let Some((ret_ty, true, bounds)) = ret_ty else {
            return Err(
                input.error("'async fn' methods must return 'Result<_>' and 'fn' methods must return 'impl Future<Output = Result<_>> + Send'")
            );
        };

        // Parse default body.
        let body = if input.peek(token::Brace) {
            let content;
            braced!(content in input);
            Some(content.call(Block::parse_within)?)
        } else {
            input.parse::<Token![;]>()?;
            None
        };

        Ok(Self {
            doc_attrs,
            serde_attrs,
            numerical_rename,
            attrs,
            ident,
            self_ref,
            args,
            ret_ty,
            bounds,
            cancel,
            pipelinable,
            pipelined_name,
            body,
        })
    }
}

impl TraitMethod {
    /// Identifier of the twin method taking a request receiver.
    ///
    /// This is the method name followed by `_pipelined`, unless a name was
    /// specified using `#[pipelinable(name)]`.
    pub fn pipelined_ident(&self) -> Ident {
        match &self.pipelined_name {
            Some(name) => name.clone(),
            None => format_ident!("{}_pipelined", &self.ident),
        }
    }

    /// The return type of the twin method taking a request receiver.
    fn pipelined_ret_ty(&self, assoc: &[AssocType]) -> TokenStream {
        let ret_ty = remove_self_type(&self.ret_ty, assoc);
        quote! { <#ret_ty as ::remoc::rtc::IsPipelinableReply>::Pipelined }
    }

    /// The type of the request receiver handed over to the twin method.
    fn pipelined_req_rx_ty(&self, assoc: &[AssocType]) -> TokenStream {
        let ret_ty = remove_self_type(&self.ret_ty, assoc);
        quote! { <#ret_ty as ::remoc::rtc::IsPipelinableReply>::ReqReceiver }
    }

    /// The bound the target object must satisfy so that the future of the twin
    /// method, which holds the target across await points, is `Send`.
    fn pipelined_self_bound(&self) -> TokenStream {
        match self.self_ref {
            SelfRef::Ref => quote! { where Self: ::std::marker::Sync },
            SelfRef::Value | SelfRef::RefMut => quote! { where Self: ::std::marker::Send },
        }
    }

    /// Twin method taking a request receiver, with its default implementation.
    ///
    /// This is a provided method of the trait, so that an implementation can
    /// override it to serve the request receiver directly instead of forwarding
    /// the requests to a client.
    pub fn pipelined_trait_method(&self, impl_future: bool, assoc: &[AssocType]) -> TokenStream {
        let Self { ident, .. } = self;
        let pipelined_ident = self.pipelined_ident();
        let ret_ty = self.pipelined_ret_ty(assoc);
        let req_rx_ty = self.pipelined_req_rx_ty(assoc);
        let self_bound = self.pipelined_self_bound();

        let self_ref = match self.self_ref {
            SelfRef::Value => quote! { self, },
            SelfRef::Ref => quote! { &self, },
            SelfRef::RefMut => quote! { &mut self, },
        };

        let mut args = quote! {};
        let mut call_args = quote! {};
        for NamedArg { ident, ty, .. } in &self.args {
            let ty = remove_self_type(ty, assoc);
            args.append_all(quote! { #ident : #ty , });
            call_args.append_all(quote! { #ident , });
        }

        let doc = format!(
            "Calls [`{ident}`](Self::{ident}) and lets the returned client execute the requests of the provided request receiver.\n\n\
             The client is returned once all requests have been executed, i.e. once every client connected to the request receiver `__req_rx` has been dropped. 
             The return value is `None` when the object was consumed by a method taking `self` by value.\n\n\
             The default implementation [forwards](::remoc::rtc::ReqReceiver::forward) the requests to the client returned by [`{ident}`](Self::{ident})."
        );

        let body = quote! {
            let __client = self.#ident(#call_args).await?;
            let __client = ::remoc::rtc::ReqReceiver::forward(__req_rx, __client)
                .await
                .map_err(::remoc::rtc::CallError::from)?;
            ::std::result::Result::Ok(__client)
        };

        if impl_future {
            quote! {
                #[doc=#doc]
                fn #pipelined_ident ( #self_ref #args __req_rx: #req_rx_ty )
                    -> impl ::std::future::Future<Output = #ret_ty> + ::std::marker::Send
                #self_bound
                {
                    async move { #body }
                }
            }
        } else {
            quote! {
                #[doc=#doc]
                async fn #pipelined_ident ( #self_ref #args __req_rx: #req_rx_ty ) -> #ret_ty
                #self_bound
                { #body }
            }
        }
    }

    /// Method definition within trait (without argument attributes).
    pub fn trait_method(&self, impl_future: bool) -> TokenStream {
        let Self { doc_attrs, attrs, ident, ret_ty, .. } = self;
        let doc_attrs = attribute_tokens(doc_attrs);
        let attrs = attribute_tokens(attrs);

        // Build argument list.
        let mut args = quote! {};

        // Self argument.
        let self_ref = match self.self_ref {
            SelfRef::Value => quote! {self,},
            SelfRef::Ref => quote! {&self,},
            SelfRef::RefMut => quote! {&mut self,},
        };
        args.append_all(self_ref);

        // Request arguments.
        for NamedArg { attrs: _, ident, ty } in &self.args {
            args.append_all(quote! { #ident : #ty , });
        }

        // Body.
        let body_opt = match &self.body {
            Some(stmts) => {
                let mut body = quote! {};
                body.append_all(stmts);
                if impl_future {
                    quote! { { async move { #body } } }
                } else {
                    quote! { { #body } }
                }
            }
            None => quote! { ; },
        };

        let sig = if impl_future {
            let bounds = if self.bounds.is_empty() {
                quote! {}
            } else {
                let bounds = &self.bounds;
                quote! { + #bounds }
            };
            quote! { #doc_attrs #attrs fn #ident ( #args ) -> impl ::std::future::Future<Output = #ret_ty> + ::std::marker::Send #bounds }
        } else {
            quote! { #doc_attrs #attrs async fn #ident ( #args ) -> #ret_ty }
        };

        quote! {
            #sig
            #body_opt
        }
    }

    /// Entry within request enum.
    pub fn request_enum_entry(&self, assoc: &[AssocType]) -> TokenStream {
        let ident = to_pascal_case(&self.ident);
        let ret_ty = remove_self_type(&self.ret_ty, assoc);

        // When the user renames the request enum variant or one of its fields to a
        // numerical identifier, they opted into the compact serialized representation
        // for this method. Thus the reply channel field also uses the numerical name
        // reserved by remoc.
        let reply_tx_rename = self.numerical_rename.then(|| quote! { #[serde(rename = #REPLY_TX_NAME)] });

        let reply_tx_ty = if self.pipelinable {
            quote! { ::remoc::rtc::PipelinableReplyTo<#ret_ty, Codec> }
        } else {
            quote! { ::remoc::rtc::ReplyTo<#ret_ty, Codec> }
        };

        let mut entries = quote! {
            #[doc="Reply channel for sending the result of the method invocation.\n\n"]
            #[doc="The channel is closed when the calling async method is cancelled "]
            #[doc="or a connection error occurs."]
            #reply_tx_rename
            __reply_tx: #reply_tx_ty,
        };

        for NamedArg { attrs, ident, ty } in &self.args {
            if !attrs.iter().any(|attr| attr.path().is_ident("doc")) {
                entries.append_all(quote! {
                    #[doc = concat!(stringify!(#ident), " parameter")]
                });
            }

            let attrs = attribute_tokens(attrs);
            let ty = remove_self_type(ty, assoc);
            entries.append_all(quote! {
                #attrs
                #ident : #ty ,
            });
        }

        let doc_attrs = attribute_tokens(&self.doc_attrs);
        let serde_attrs = attribute_tokens(&self.serde_attrs);
        quote! { #doc_attrs #serde_attrs #ident {#entries} , }
    }

    /// Enum match discriminator and dispatch code.
    pub fn dispatch_discriminator(&self) -> TokenStream {
        let ident = &self.ident;
        let enum_ident = to_pascal_case(ident);

        // Build call argument list.
        let mut args = quote! {};
        for NamedArg { ident: arg_ident, .. } in &self.args {
            args.append_all(quote! { #arg_ident, });
        }

        // Invokes `method` with the request arguments and replies on `reply_tx`.
        let invoke = |method: TokenStream, reply_tx: TokenStream, extra_args: TokenStream| {
            if self.cancel {
                quote! {
                    ::remoc::rtc::select! {
                        biased;
                        () = #reply_tx.closed() => (),
                        result = #method(#args #extra_args) => {
                            ::remoc::rtc::send_reply(#reply_tx, &__err_tx, __guard, result).await;
                        }
                    }
                }
            } else {
                quote! {
                    let result = #method(#args #extra_args).await;
                    ::remoc::rtc::send_reply(#reply_tx, &__err_tx, __guard, result).await;
                }
            }
        };

        let call = if self.pipelinable {
            let pipelined_ident = self.pipelined_ident();
            let normal = invoke(quote! { __target.#ident }, quote! { __reply_tx }, quote! {});
            let pipeline = invoke(quote! { __target.#pipelined_ident }, quote! { reply_tx }, quote! { req_rx, });
            quote! {
                match __reply_tx {
                    ::remoc::rtc::PipelinableReplyTo::Normal(__reply_tx) => { #normal }
                    ::remoc::rtc::PipelinableReplyTo::Pipeline { req_rx, reply_tx } => { #pipeline }
                }
            }
        } else {
            invoke(quote! { __target.#ident }, quote! { __reply_tx }, quote! {})
        };

        // Generate match clause.
        quote! {
            Self :: #enum_ident { #args __reply_tx } => {
                async move { #call }.boxed()
            },
        }
    }

    /// Match clause returning the method name for the `ReqEnum::method_name` implementation.
    pub fn method_name_clause(&self) -> TokenStream {
        let enum_ident = to_pascal_case(&self.ident);
        let name = self.ident.to_string();
        quote! {
            Self :: #enum_ident { .. } => #name,
        }
    }

    /// Client method implementation.
    pub fn client_method(
        &self, req_value: &Ident, req_ref: &Ident, req_ref_mut: &Ident, assoc: &[AssocType],
    ) -> TokenStream {
        let Self { ident, self_ref, .. } = self;
        let ret_ty = remove_self_type(&self.ret_ty, assoc);

        // Self reference and request enum.
        let (self_ref, req_enum, req_type) = match self_ref {
            SelfRef::Value => (quote! { self }, req_value, quote! { Value }),
            SelfRef::Ref => (quote! { &self }, req_ref, quote! { Ref }),
            SelfRef::RefMut => (quote! { &mut self }, req_ref_mut, quote! { RefMut }),
        };
        let req_case = to_pascal_case(ident);

        // Argument and request enum entry list.
        let mut args = quote! {};
        let mut entries = quote! {};
        for NamedArg { ident, ty, .. } in &self.args {
            let ty = remove_self_type(ty, assoc);
            args.append_all(quote! { #ident : #ty , });
            entries.append_all(quote! { #ident , });
        }

        // A pipelinable method wraps the reply channel, so that a request receiver can
        // be handed over in its place.
        let reply_to = if self.pipelinable {
            quote! { ::remoc::rtc::PipelinableReplyTo::Normal(reply_tx) }
        } else {
            quote! { ::remoc::rtc::ReplyTo::from(reply_tx) }
        };

        let pipelined_method = self.pipelinable.then(|| self.pipelined_client_method(req_enum, &req_type, assoc));

        quote! {
            async fn #ident (#self_ref, #args) -> #ret_ty {
                let (reply_tx, reply_rx) = ::remoc::rtc::reply_channel(self.max_reply_size);

                let req_value = #req_enum :: #req_case { __reply_tx: #reply_to, #entries };
                let req = ::remoc::rtc::Req::#req_type(req_value);

                let mut guard = match self.monitor.pre_call(&req).await {
                    ::remoc::rtc::CallDecision::Pass => ::std::boxed::Box::new(::remoc::rtc::DefaultGuard),
                    ::remoc::rtc::CallDecision::Guard(guard) => guard,
                    ::remoc::rtc::CallDecision::Drop => return Err(::remoc::rtc::CallError::Dropped.into()),
                };

                self.req_tx.send(req).await.map_err(::remoc::rtc::CallError::from)?;

                match reply_rx.await {
                    Ok(reply) => {
                        let reply: #ret_ty = ::std::convert::Into::into(reply);
                        if reply.is_err() {
                            guard.failed();
                        }
                        reply
                    }
                    Err(err) => {
                        guard.reply_failed(&err);
                        Err(::remoc::rtc::CallError::from(err).into())
                    }
                }
            }

            #pipelined_method
        }
    }

    /// Implementation of the twin method taking a request receiver for the client.
    ///
    /// It sends the same request as the normal method, but hands the request receiver
    /// over in place of the reply channel.
    fn pipelined_client_method(
        &self, req_enum: &Ident, req_type: &TokenStream, assoc: &[AssocType],
    ) -> TokenStream {
        let pipelined_ident = self.pipelined_ident();
        let ret_ty = self.pipelined_ret_ty(assoc);
        let req_rx_ty = self.pipelined_req_rx_ty(assoc);
        let self_bound = self.pipelined_self_bound();
        let req_case = to_pascal_case(&self.ident);

        let self_ref = match self.self_ref {
            SelfRef::Value => quote! { self, },
            SelfRef::Ref => quote! { &self, },
            SelfRef::RefMut => quote! { &mut self, },
        };

        let mut args = quote! {};
        let mut entries = quote! {};
        for NamedArg { ident, ty, .. } in &self.args {
            let ty = remove_self_type(ty, assoc);
            args.append_all(quote! { #ident : #ty , });
            entries.append_all(quote! { #ident , });
        }

        quote! {
            async fn #pipelined_ident (#self_ref #args __req_rx: #req_rx_ty) -> #ret_ty
            #self_bound
            {
                let (reply_tx, reply_rx) = ::remoc::rtc::reply_channel(self.max_reply_size);

                let req_value = #req_enum :: #req_case {
                    __reply_tx: ::remoc::rtc::PipelinableReplyTo::Pipeline {
                        req_rx: __req_rx, reply_tx,
                    },
                    #entries
                };
                let req = ::remoc::rtc::Req::#req_type(req_value);

                let mut guard = match self.monitor.pre_call(&req).await {
                    ::remoc::rtc::CallDecision::Pass => ::std::boxed::Box::new(::remoc::rtc::DefaultGuard),
                    ::remoc::rtc::CallDecision::Guard(guard) => guard,
                    ::remoc::rtc::CallDecision::Drop => return Err(::remoc::rtc::CallError::Dropped.into()),
                };

                self.req_tx.send(req).await.map_err(::remoc::rtc::CallError::from)?;

                match reply_rx.await {
                    Ok(reply) => {
                        let reply: #ret_ty = ::std::convert::Into::into(reply);
                        if ::remoc::rtc::IsReply::is_error(&reply) {
                            guard.failed();
                        }
                        reply
                    }
                    Err(err) => {
                        guard.reply_failed(&err);
                        Err(::remoc::rtc::CallError::from(err).into())
                    }
                }
            }
        }
    }
}
