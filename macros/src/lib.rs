//! `#[derive(Trace)]`：逐个字段追踪（`trace`）与重定位（`relocate`，疏散回收时改写边）；`NO_EDGES` 是全部字段的与。
//!
//! 缺省还会检查类型没有实现 `Drop`（析构顺序不定，`Drop` 里可能碰到已经析构的对象）；
//! 确实需要时写 `#[trace(unsafe_drop)]`，由你保证 `Drop` 里不碰任何 `Gc`。
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, Ident, Type, parse_macro_input, parse_quote};

/// `#[derive(Shared)]`：能放进公开堆的类型（共享内存上、可能被别的进程读和回收）。要求 `#[repr(C)]`（跨进程、跨二进制布局才一致），
/// 只支持结构体；生成指针图（每个 `CrossHeapSlot` 字段的偏移，嵌套类型与定长数组展开）。没有析构由 `CrossHeapMutator::alloc` 在编译期检查。
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
    /// 有没有 `#[repr(C)]`（可以和 `align(..)` 等写在一起）。
    fn repr_c(&self) -> syn::Result<bool> {
        let mut found = false;
        for attr in self.input.attrs.iter().filter(|a| a.path().is_ident("repr")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("C") {
                    found = true;
                }
                // align(N) 等带参数的：吃掉参数
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
            return Err(syn::Error::new(Span::call_site(), "derive(Shared) 只支持结构体"));
        };
        if !self.repr_c()? {
            return Err(syn::Error::new(Span::call_site(), "公开堆的类型要 #[repr(C)]（跨进程、跨二进制布局才一致）"));
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
            // SAFETY: 逐个字段报指针槽
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

/// 一组字段的匹配模式与类型（结构体一组，枚举每个变体一组）。
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
                    Err(meta.error("只认 #[trace(unsafe_drop)]"))
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
            Data::Union(_) => Err(syn::Error::new(Span::call_site(), "union 不能 derive(Trace)")),
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
                    /// 有 `Drop` 的类型会和下面的总括实现冲突
                    trait MustNotImplDrop {}
                    #[allow(drop_bounds)]
                    impl<T: ::core::ops::Drop> MustNotImplDrop for T {}
                    impl #impl_generics MustNotImplDrop for #name #ty_generics #where_clause {}
                };
            }
        };
        Ok(quote! {
            // SAFETY: 逐个字段追踪
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
