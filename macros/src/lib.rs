//! `#[derive(Trace)]`: traces (`trace`) and relocates (`relocate`, rewriting edges during evacuating collections) field by field; `NO_EDGES` is the AND over all fields.
//!
//! By default it also checks that the type does not implement `Drop` (finalization order is unspecified, so `Drop` could touch an already finalized object);
//! when you really need one, write `#[trace(unsafe_drop)]` and guarantee that `Drop` touches no `Gc`.
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, Ident, Type, parse_macro_input, parse_quote};

/// `#[derive(Shared)]`: a type that can live in a public heap (on shared memory, possibly read and collected by other processes). Requires `#[repr(C)]` (so the layout agrees across processes and binaries);
/// structs only. Generates the pointer map (the offset of every pointer-slot field, expanding nested types and fixed-size arrays). `CrossHeapMutator::alloc` checks at compile time that there is no destructor.
#[proc_macro_derive(Shared)]
pub fn derive_shared(input: TokenStream) -> TokenStream {
    SharedDerive::from(parse_macro_input!(input as DeriveInput)).expand().unwrap_or_else(syn::Error::into_compile_error).into()
}

struct SharedDerive {
    input: DeriveInput,
}

impl From<DeriveInput> for SharedDerive {
    fn from(input: DeriveInput) -> Self {
        Self { input }
    }
}

impl SharedDerive {
    /// Whether there is `#[repr(C)]` (possibly together with `align(..)` and the like).
    fn repr_c(&self) -> syn::Result<bool> {
        let mut found = false;
        for attr in self.input.attrs.iter().filter(|a| a.path().is_ident("repr")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("C") {
                    found = true;
                }
                // align(N) and other forms with arguments: consume the arguments
                if meta.input.peek(syn::token::Paren) {
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let _: Tokens = content.parse()?;
                }
                Ok(())
            })?;
        }
        Ok(found)
    }

    fn expand(&self) -> syn::Result<Tokens> {
        let name = &self.input.ident;
        let Data::Struct(s) = &self.input.data else {
            return Err(syn::Error::new(Span::call_site(), "derive(Shared) only supports structs"));
        };
        if !self.repr_c()? {
            return Err(syn::Error::new(Span::call_site(), "public heap types need #[repr(C)] (so the layout agrees across processes and binaries)"));
        }
        let members: Vec<syn::Member> = s
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| f.ident.clone().map_or_else(|| syn::Member::Unnamed(syn::Index::from(i)), syn::Member::Named))
            .collect();
        let types: Vec<&Type> = s.fields.iter().map(|f| &f.ty).collect();
        let mut generics = self.input.generics.clone();
        let params: Vec<Ident> = generics.type_params().map(|p| p.ident.clone()).collect();
        let where_clause = generics.make_where_clause();
        for p in params {
            where_clause.predicates.push(parse_quote!(#p: ::paragc::Shared));
        }
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        Ok(quote! {
            // SAFETY: reports the pointer slots field by field
            unsafe impl #impl_generics ::paragc::Shared for #name #ty_generics #where_clause {
                const NO_EDGES: bool = true #(&& <#types as ::paragc::Shared>::NO_EDGES)*;

                #[allow(unused_variables)]
                fn edges(__map: &mut ::paragc::EdgeMap, __at: u32) {
                    #(<#types as ::paragc::Shared>::edges(__map, __at + ::paragc::EdgeMap::offset(::core::mem::offset_of!(Self, #members)));)*
                }
            }
        })
    }
}

#[proc_macro_derive(Trace, attributes(trace))]
pub fn derive_trace(input: TokenStream) -> TokenStream {
    Derive::from(parse_macro_input!(input as DeriveInput)).expand().unwrap_or_else(syn::Error::into_compile_error).into()
}

struct Derive {
    input: DeriveInput,
}

/// The match pattern and types of a group of fields (one group for a struct, one per variant for an enum).
struct Arm {
    pattern: Tokens,
    bindings: Vec<Ident>,
}

impl From<DeriveInput> for Derive {
    fn from(input: DeriveInput) -> Self {
        Self { input }
    }
}

impl Derive {
    /// `#[trace(unsafe_drop)]`
    fn unsafe_drop(&self) -> syn::Result<bool> {
        let mut found = false;
        for attr in self.input.attrs.iter().filter(|a| a.path().is_ident("trace")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("unsafe_drop") {
                    found = true;
                    Ok(())
                } else {
                    Err(meta.error("only #[trace(unsafe_drop)] is recognized"))
                }
            })?;
        }
        Ok(found)
    }

    fn arm(path: &Tokens, fields: &Fields) -> Arm {
        let bindings: Vec<Ident> = (0..fields.len()).map(|i| format_ident!("__f{i}")).collect();
        let pattern = match fields {
            Fields::Named(named) => {
                let field_names = named.named.iter().map(|f| f.ident.as_ref());
                quote!(#path { #(#field_names: #bindings),* })
            }
            Fields::Unnamed(_) => quote!(#path( #(#bindings),* )),
            Fields::Unit => quote!(#path),
        };
        Arm { pattern, bindings }
    }

    fn arms_and_types(&self) -> syn::Result<(Vec<Arm>, Vec<Type>)> {
        match &self.input.data {
            Data::Struct(s) => Ok((vec![Self::arm(&quote!(Self), &s.fields)], s.fields.iter().map(|f| f.ty.clone()).collect())),
            Data::Enum(e) => Ok((
                e.variants
                    .iter()
                    .map(|v| {
                        let name = &v.ident;
                        Self::arm(&quote!(Self::#name), &v.fields)
                    })
                    .collect(),
                e.variants.iter().flat_map(|v| v.fields.iter().map(|f| f.ty.clone())).collect(),
            )),
            Data::Union(_) => Err(syn::Error::new(Span::call_site(), "unions cannot derive(Trace)")),
        }
    }

    fn expand(&self) -> syn::Result<Tokens> {
        let name = &self.input.ident;
        let mut generics = self.input.generics.clone();
        let params: Vec<Ident> = generics.type_params().map(|p| p.ident.clone()).collect();
        let where_clause = generics.make_where_clause();
        for p in params {
            where_clause.predicates.push(parse_quote!(#p: ::paragc::Trace));
        }
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

        let (arms, types) = self.arms_and_types()?;
        let trace_arms = arms.iter().map(|Arm { pattern, bindings }| quote!(#pattern => { #(::paragc::Trace::trace(#bindings, __t);)* }));
        let relocate_arms = arms.iter().map(|Arm { pattern, bindings }| quote!(#pattern => { #(::paragc::Trace::relocate(#bindings, __t);)* }));
        let no_drop = if self.unsafe_drop()? {
            quote!()
        } else {
            quote! {
                const _: () = {
                    /// A type with `Drop` conflicts with the blanket impl below
                    trait MustNotImplDrop {}
                    #[allow(drop_bounds)]
                    impl<T: ::core::ops::Drop> MustNotImplDrop for T {}
                    impl #impl_generics MustNotImplDrop for #name #ty_generics #where_clause {}
                };
            }
        };
        Ok(quote! {
            // SAFETY: traces field by field
            unsafe impl #impl_generics ::paragc::Trace for #name #ty_generics #where_clause {
                const NO_EDGES: bool = true #(&& <#types as ::paragc::Trace>::NO_EDGES)*;

                #[allow(unused_variables)]
                fn trace(&self, __t: &mut ::paragc::Tracer<'_>) {
                    if <Self as ::paragc::Trace>::NO_EDGES {
                        return;
                    }
                    match self {
                        #(#trace_arms)*
                    }
                }

                #[allow(unused_variables)]
                fn relocate(&mut self, __t: &mut ::paragc::Tracer<'_>) {
                    if <Self as ::paragc::Trace>::NO_EDGES {
                        return;
                    }
                    match self {
                        #(#relocate_arms)*
                    }
                }
            }
            #no_drop
        })
    }
}
