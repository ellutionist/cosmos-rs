//! `#[cosmos::userdata]`：把 impl 块里的方法自动桥接进 Lua metatable。
//!
//! 用法：
//! ```ignore
//! #[cosmos::userdata(name = "network.http.client")]
//! impl HttpClient {
//!     fn new(opts: Option<ClientOptions>) -> Self { ... }
//! }
//! ```
//! 默认路径取类型短名，name 指定相对 cosmos 的模块路径。
//!
//! 每个方法生成一个 Rust LuaCallable 适配，负责组合参数和返回值转换；
//! C ABI、panic 捕获及异步挂起由 cosmos 内部的共用 trampoline 处理，并生成
//! `impl cosmos::userdata::UserData` 把它们注册进 MethodTable。
//! `__gc` 不需要宏处理：运行时用 `ptr::drop_in_place` 统一生成，
//! 对没有 Drop 的类型同样是合法空操作。

#![warn(unreachable_pub)]

use proc_macro::TokenStream;
use proc_macro2::Literal;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, spanned::Spanned, Data, DataStruct, DeriveInput, Field, Fields, FnArg,
    ImplItem, ItemImpl, LitByteStr, LitStr, PatType, Type,
};

#[proc_macro_attribute]
pub fn userdata(attr: TokenStream, item: TokenStream) -> TokenStream {
    let name_override = match parse_userdata_attr(attr.into()) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };
    let imp = parse_macro_input!(item as ItemImpl);

    let mut errors: Vec<syn::Error> = Vec::new();

    if imp.trait_.is_some() {
        errors.push(syn::Error::new(
            imp.span(),
            "#[userdata] requires an inherent impl block",
        ));
    }
    if !imp.generics.params.is_empty() || imp.generics.where_clause.is_some() {
        errors.push(syn::Error::new(
            imp.generics.span(),
            "#[userdata] does not support generics",
        ));
    }

    let ty = &imp.self_ty;
    let ty_label = type_label(ty);
    let lua_name = syn::LitStr::new(name_override.as_deref().unwrap_or(&ty_label), ty.span());

    let mut trampolines = Vec::new();
    let mut registrations = Vec::new();
    for item in &imp.items {
        match item {
            ImplItem::Fn(f) => match gen_method(ty, &ty_label, f) {
                Ok((c_name, tramp_ident, tramp)) => {
                    registrations.push(quote! { methods.add::<#tramp_ident>(#c_name); });
                    trampolines.push(tramp);
                }
                Err(e) => errors.push(e),
            },
            other => errors.push(syn::Error::new(
                other.span(),
                "#[userdata] impl blocks may only contain methods",
            )),
        }
    }

    if !errors.is_empty() {
        return errors
            .into_iter()
            .map(|e| e.to_compile_error())
            .collect::<proc_macro2::TokenStream>()
            .into();
    }

    let expanded = quote! {
        #imp

        const _: () = {
            #(#trampolines)*

            impl cosmos::userdata::UserData for #ty {
                fn lua_name() -> &'static str {
                    #lua_name
                }

                fn add_methods(methods: &mut cosmos::userdata::MethodTable<Self>) {
                    #(#registrations)*
                }
            }
        };
    };
    expanded.into()
}

/// `#[cosmos::userdata(name = "network.http.client")]`：Lua require 路径覆盖，
/// 默认是类型短名，显式 name 相对 cosmos 根模块解析。
fn parse_userdata_attr(attr: proc_macro2::TokenStream) -> syn::Result<Option<String>> {
    if attr.is_empty() {
        return Ok(None);
    }
    let nv: syn::MetaNameValue = syn::parse2(attr)?;
    if !nv.path.is_ident("name") {
        return Err(syn::Error::new(
            nv.path.span(),
            "expected `name = \"module.path\"`",
        ));
    }
    match &nv.value {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => Ok(Some(s.value())),
        other => Err(syn::Error::new(other.span(), "expected a string literal")),
    }
}

fn err<T>(span: &dyn Spanned, msg: &str) -> syn::Result<T> {
    Err(syn::Error::new(span.span(), msg))
}

/// trampoline 命名用：路径类型取最后一段
fn type_label(ty: &Type) -> String {
    match ty {
        Type::Path(p) if p.qself.is_none() => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "T".into()),
        _ => "T".into(),
    }
}

/// 生成单个方法的 Rust 调用适配。返回方法名和用于单态化内部 trampoline 的类型。
fn gen_method(
    ty: &Type,
    ty_label: &str,
    f: &syn::ImplItemFn,
) -> syn::Result<(Literal, proc_macro2::Ident, proc_macro2::TokenStream)> {
    let sig = &f.sig;
    let method = &sig.ident;
    let is_async = sig.asyncness.is_some();

    if sig.unsafety.is_some() {
        return err(&f.sig, "unsafe methods are not supported");
    }
    if sig.abi.is_some() {
        return err(&f.sig, "extern methods are not supported");
    }
    if sig.variadic.is_some() {
        return err(&f.sig, "variadic methods are not supported");
    }

    // 静态方法（无 self 接收者）：不做 self 检查，参数从栈 1 开始；
    // 实例方法：首参是 Receiver，check 后参数从栈 2 开始。
    // &mut self 的守卫需要可变绑定（RefMut 的 DerefMut）
    let (is_static, self_binding) = match sig.inputs.first() {
        Some(FnArg::Receiver(r)) => {
            if r.colon_token.is_some() {
                return err(&r.ty, "typed self is not supported");
            }
            let binding = match (&r.reference, &r.mutability) {
                (Some(_), None) => {
                    quote!(let this = invocation.argument(1).borrow::<#ty>()?;)
                }
                (Some(_), Some(_)) => {
                    quote!(let mut this = invocation.argument(1).borrow_mut::<#ty>()?;)
                }
                (None, _) => {
                    // 拥有型接收者经 FromLua 取得独立句柄，异步等待不占用 userdata 借用。
                    quote!(let this = invocation.argument(1).read::<#ty>()?;)
                }
            };
            (false, binding)
        }
        Some(FnArg::Typed(_)) => (true, quote!()),
        // 无参静态方法（如 TlsContext.new()）：参数从栈 1 开始，但没有要转换的实参
        None => (true, quote!()),
    };

    let mut arg_idents = Vec::new();
    let mut arg_conversions = Vec::new();
    let stack_base: i32 = if is_static { 1 } else { 2 };
    let mut inputs = sig.inputs.iter();
    if !is_static {
        inputs.next(); // 跳过 receiver
    }
    for (arg_n, arg) in inputs.enumerate() {
        let FnArg::Typed(PatType { ty: arg_ty, .. }) = arg else {
            return err(arg, "self must be the first argument");
        };
        if is_async && matches!(arg_ty.as_ref(), Type::Reference(_)) {
            return err(
                arg_ty,
                "async methods cannot borrow Lua arguments across yield; \
                     use owned types (e.g. String instead of &str)",
            );
        }
        let stack_idx = stack_base + arg_n as i32;
        let ident = format_ident!("arg{arg_n}");
        arg_idents.push(ident.clone());
        arg_conversions.push(quote! {
            let #ident = invocation.argument(#stack_idx as usize).read::<#arg_ty>()?;
        });
    }

    // 静态方法按关联函数调用 Type::method(...)，实例方法走 this.method(...)
    let call = if is_static {
        quote!(#ty::#method(#(#arg_idents),*))
    } else {
        quote!(this.#method(#(#arg_idents),*))
    };

    let tramp_ident = format_ident!("__cosmos_m_{}_{}", ty_label, method);
    // 静态字段名在编译时构造 CStr，注册时无需临时分配 CString。
    let c_name = {
        let cs = std::ffi::CString::new(method.to_string()).unwrap();
        Literal::c_string(&cs)
    };

    let execute = if is_async {
        quote!(Ok(invocation.suspend(async move { #call.await })))
    } else {
        quote! {
            let ret = #call;
            Ok(invocation.returns(ret))
        }
    };
    let tramp = quote! {
        #[allow(non_camel_case_types)]
        struct #tramp_ident;
        unsafe impl cosmos::userdata::LuaCallable for #tramp_ident {
            // 参数及返回类型决定是否需要 unsafe，零参数异步调用也使用此统一模板。
            #[allow(unused_unsafe)]
            fn call(invocation: &mut cosmos::userdata::Call<'_>) -> ::core::result::Result<i32, ::std::string::String> {
                unsafe {
                    #self_binding
                    #(#arg_conversions)*
                    #execute
                }
            }
        }
    };

    Ok((c_name, tramp_ident, tramp))
}

// ---------------------------------------------------------------------------
// #[derive(FromLua)] / #[derive(PushLua)]：Lua table ↔ Rust struct
//
// 字段属性（与 serde 的分工类似）：
//   #[lua(name = "external_key")]   Lua 侧键名，默认等于 Rust 字段名
//   #[lua(default)]                 缺省时 Default::default()
//   #[lua(default = expr)]          缺省时用 expr（值或函数调用）
//   #[lua(default_fn = path)]       缺省时调用 path()（返回字段类型）
//
// 结构体属性（用于整张表的缺省，字段级优先）：
//   #[lua(default)]                 缺省字段取 Self::default() 的同名字段
//   #[lua(default = expr)]          缺省字段取 expr 得到的 Self 的同名字段
//   #[lua(default_fn = path)]       缺省字段取 path() 得到的 Self 的同名字段
//
// 缺省且没有 default/default_fn、结构体也没有默认的字段读表时报错；
// 未知键忽略；非字符串键跳过（绝不对键做会改写键值的转换，避免破坏表遍历状态）。
// 键比较直接用字节切片，不为每个键分配 Rust String。
// ---------------------------------------------------------------------------

enum FieldDefault {
    Default,
    Expr(syn::Expr),
    Fn(syn::Path),
}

struct LuaField {
    ident: syn::Ident,
    ty: Type,
    key: String,
    default: Option<FieldDefault>,
}

fn lua_fields(input: &DeriveInput) -> syn::Result<Vec<LuaField>> {
    let Data::Struct(DataStruct {
        fields: Fields::Named(named),
        ..
    }) = &input.data
    else {
        return err(&input.ident, "expected a struct with named fields");
    };
    named.named.iter().map(parse_lua_field).collect()
}

/// 结构体级 `#[lua(default)]` / `#[lua(default = expr)]` / `#[lua(default_fn = path)]`
fn parse_container_default(attrs: &[syn::Attribute]) -> syn::Result<Option<FieldDefault>> {
    let mut default = None;
    for attr in attrs {
        if !attr.path().is_ident("lua") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("default") {
                if default.is_some() {
                    return Err(meta.error("duplicate `default` / `default_fn`"));
                }
                default = Some(if meta.input.peek(syn::Token![=]) {
                    FieldDefault::Expr(meta.value()?.parse()?)
                } else {
                    FieldDefault::Default
                });
                Ok(())
            } else if meta.path.is_ident("default_fn") {
                if default.is_some() {
                    return Err(meta.error("duplicate `default` / `default_fn`"));
                }
                default = Some(FieldDefault::Fn(meta.value()?.parse()?));
                Ok(())
            } else {
                Err(meta
                    .error("unknown struct-level lua attribute (expected default / default_fn)"))
            }
        })?;
    }
    Ok(default)
}

fn parse_lua_field(field: &Field) -> syn::Result<LuaField> {
    let Some(ident) = field.ident.clone() else {
        return err(&field.ty, "unnamed fields are not supported");
    };
    let mut key = ident.to_string();
    let mut default = None;
    for attr in &field.attrs {
        if !attr.path().is_ident("lua") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                key = meta.value()?.parse::<LitStr>()?.value();
                Ok(())
            } else if meta.path.is_ident("default") {
                if default.is_some() {
                    return Err(meta.error("duplicate `default` / `default_fn`"));
                }
                default = Some(if meta.input.peek(syn::Token![=]) {
                    FieldDefault::Expr(meta.value()?.parse()?)
                } else {
                    FieldDefault::Default
                });
                Ok(())
            } else if meta.path.is_ident("default_fn") {
                if default.is_some() {
                    return Err(meta.error("duplicate `default` / `default_fn`"));
                }
                default = Some(FieldDefault::Fn(meta.value()?.parse()?));
                Ok(())
            } else {
                Err(meta.error("unknown lua attribute (expected name / default / default_fn)"))
            }
        })?;
    }
    Ok(LuaField {
        ident,
        ty: field.ty.clone(),
        key,
        default,
    })
}

#[proc_macro_derive(FromLua, attributes(lua))]
pub fn derive_from_lua(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_from_lua(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand_from_lua(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    if matches!(&input.data, Data::Enum(_)) {
        return expand_from_lua_enum(input);
    }
    let name = &input.ident;
    let generics = from_lua_generics(input);
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let (_, ty_generics, _) = input.generics.split_for_impl();
    let fields = lua_fields(input)?;
    let container_default = parse_container_default(&input.attrs)?;
    let span = proc_macro2::Span::call_site();

    // 只有确实有字段要靠结构体级默认兜底时才构造默认实例，避免无用变量警告
    let needs_container_default =
        container_default.is_some() && fields.iter().any(|f| f.default.is_none());

    let seen: Vec<syn::Ident> = (0..fields.len())
        .map(|i| syn::Ident::new(&format!("__cosmos_field_{i}"), span))
        .collect();

    let decls = fields.iter().zip(&seen).map(|(f, s)| {
        let ty = &f.ty;
        quote!(let mut #s: ::core::option::Option<#ty> = ::core::option::Option::None;)
    });

    let checks = fields.iter().zip(&seen).map(|(f, s)| {
        let key = LitByteStr::new(f.key.as_bytes(), span);
        let ty = &f.ty;
        quote! {
            if key == &#key[..] {
                #s = ::core::option::Option::Some(value.read::<#ty>()?);
            }
        }
    });

    let values = fields.iter().zip(&seen).map(|(f, s)| {
        let ident = &f.ident;
        let missing = match &f.default {
            Some(FieldDefault::Default) => quote!(::core::default::Default::default()),
            Some(FieldDefault::Expr(expr)) => quote!(#expr),
            Some(FieldDefault::Fn(path)) => quote!(#path()),
            // 字段没写默认值：结构体级默认存在就取默认实例的同名字段
            None if needs_container_default => quote!(__cosmos_defaults.#ident),
            None => {
                let key = &f.key;
                quote! {
                    return ::core::result::Result::Err(::std::format!(
                        "missing field `{}`", #key
                    ))
                }
            }
        };
        quote! {
            #ident: match #s {
                ::core::option::Option::Some(v) => v,
                ::core::option::Option::None => #missing,
            }
        }
    });

    let container_init = if needs_container_default {
        let init = match container_default.as_ref().expect("checked above") {
            FieldDefault::Default => quote!(::core::default::Default::default()),
            FieldDefault::Expr(expr) => quote!(#expr),
            FieldDefault::Fn(path) => quote!(#path()),
        };
        quote!(let __cosmos_defaults: Self = #init;)
    } else {
        quote!()
    };

    let scan = if fields.is_empty() {
        quote!()
    } else {
        quote! {
            __cosmos_table.visit(|key, value| {
                #(#checks)*
                Ok(())
            })?;
        }
    };

    Ok(quote! {
        impl #impl_generics cosmos::userdata::FromLua<'__cosmos_lua> for #name #ty_generics #where_clause {
            unsafe fn from_lua(value: cosmos::userdata::Value<'__cosmos_lua>) -> ::core::result::Result<Self, ::std::string::String> {
                let __cosmos_table = value.table()?;
                #(#decls)*
                #container_init
                #scan
                Ok(Self { #(#values),* })
            }
        }
    })
}

fn from_lua_generics(input: &DeriveInput) -> syn::Generics {
    let mut generics = input.generics.clone();
    generics.params.insert(0, syn::parse_quote!('__cosmos_lua));
    let fields: Vec<_> = match &input.data {
        Data::Struct(data) => data.fields.iter().collect(),
        Data::Enum(data) => data
            .variants
            .iter()
            .flat_map(|variant| variant.fields.iter())
            .collect(),
        Data::Union(_) => Vec::new(),
    };
    for field in fields {
        let ty = &field.ty;
        let type_tokens = quote!(#ty).to_string();
        // 已有高阶读取约束足以覆盖新生命周期；重复添加会使 Rust 的 trait 选择产生歧义。
        let is_read_bound = |bound: &syn::TypeParamBound| {
            matches!(
                bound,
                syn::TypeParamBound::Trait(bound)
                    if bound.lifetimes.is_some()
                        && bound.path.segments.last().is_some_and(|segment| segment.ident == "FromLua")
            )
        };
        let parameter_bound = input.generics.type_params().any(|parameter| {
            parameter.ident == type_tokens && parameter.bounds.iter().any(&is_read_bound)
        });
        let where_bound = input.generics.where_clause.as_ref().is_some_and(|clause| {
            clause.predicates.iter().any(|predicate| match predicate {
                syn::WherePredicate::Type(predicate) => {
                    let bounded = &predicate.bounded_ty;
                    type_tokens == quote!(#bounded).to_string()
                        && predicate.bounds.iter().any(&is_read_bound)
                }
                _ => false,
            })
        });
        if !parameter_bound && !where_bound {
            generics
                .make_where_clause()
                .predicates
                .push(syn::parse_quote!(#ty: cosmos::userdata::FromLua<'__cosmos_lua>));
        }
    }
    generics
}

/// 无标签枚举按首个 Lua 值的类型分流，分支内字段按顺序读取多个槽位。
/// 显式标注 `#[lua(type = "table")]` 等，避免试读分支带来的额外分配与副作用。
fn expand_from_lua_enum(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let Data::Enum(data) = &input.data else {
        return err(&input.ident, "#[derive(FromLua)] expected an enum");
    };
    let name = &input.ident;
    let generics = from_lua_generics(input);
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let (_, ty_generics, _) = input.generics.split_for_impl();
    let mut kinds = std::collections::HashSet::new();
    let mut arms = Vec::new();
    let mut expected = Vec::new();
    let mut fallback = None;
    for variant in &data.variants {
        let mut kind = None;
        let mut other = false;
        for attr in variant.attrs.iter().filter(|a| a.path().is_ident("lua")) {
            attr.parse_nested_meta(|meta| {
                if kind.is_some() || other {
                    return Err(meta.error("expected one Lua type selector per variant"));
                }
                if meta.path.is_ident("other") {
                    other = true;
                    return Ok(());
                }
                if !meta.path.is_ident("type") {
                    return Err(meta.error("expected `type = \"Lua type\"` or `other`"));
                }
                kind = Some(meta.value()?.parse::<LitStr>()?);
                Ok(())
            })?;
        }
        if other {
            if !matches!(variant.fields, Fields::Unit) || fallback.is_some() {
                return err(
                    variant,
                    "#[lua(other)] requires a single unit fallback variant",
                );
            }
            let vident = &variant.ident;
            fallback = Some(quote!(_ => Ok(Self::#vident)));
            continue;
        }
        let kind = kind.ok_or_else(|| {
            syn::Error::new(
                variant.span(),
                "enum variants require #[lua(type = \"Lua type\")]",
            )
        })?;
        let kind_name = kind.value();
        let constant = match kind_name.as_str() {
            "none" => quote!(cosmos::userdata::ValueKind::Missing),
            "nil" => quote!(cosmos::userdata::ValueKind::Nil),
            "boolean" => quote!(cosmos::userdata::ValueKind::Boolean),
            "number" => quote!(cosmos::userdata::ValueKind::Number),
            "string" => quote!(cosmos::userdata::ValueKind::String),
            "table" => quote!(cosmos::userdata::ValueKind::Table),
            "function" => quote!(cosmos::userdata::ValueKind::Function),
            "userdata" => quote!(cosmos::userdata::ValueKind::UserData),
            _ => return err(&kind, "unsupported Lua type"),
        };
        if !kinds.insert(kind_name.clone()) {
            return err(&kind, "duplicate Lua type in enum variants");
        }
        expected.push(kind_name);
        let vident = &variant.ident;
        let reads: Vec<_> = variant
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let ty = &field.ty;
                let offset = i;
                quote!(value.offset(#offset).read::<#ty>()?)
            })
            .collect();
        let construct = match &variant.fields {
            Fields::Unit => quote!(Self::#vident),
            Fields::Unnamed(_) => quote!(Self::#vident(#(#reads),*)),
            Fields::Named(fields) => {
                let names = fields
                    .named
                    .iter()
                    .map(|field| field.ident.as_ref().unwrap());
                quote!(Self::#vident { #(#names: #reads),* })
            }
        };
        arms.push(quote!(#constant => Ok(#construct)));
    }
    let expected = expected.join(" / ");
    let fallback = fallback.unwrap_or_else(|| {
        quote! {
            _ => Err(value.expected(#expected))
        }
    });
    Ok(quote! {
        impl #impl_generics cosmos::userdata::FromLua<'__cosmos_lua> for #name #ty_generics #where_clause {
            unsafe fn from_lua(value: cosmos::userdata::Value<'__cosmos_lua>) -> ::core::result::Result<Self, ::std::string::String> {
                match value.kind() {
                    #(#arms,)*
                    #fallback,
                }
            }
        }
    })
}

#[proc_macro_derive(PushLua, attributes(lua))]
pub fn derive_push_lua(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_push_lua(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand_push_lua(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    if matches!(&input.data, Data::Enum(_)) {
        return expand_push_lua_enum(input);
    }
    let name = &input.ident;
    let fields = lua_fields(input)?;
    let n = fields.len();
    let mutable = (!fields.is_empty()).then(|| quote!(mut));

    let sets = fields.iter().map(|f| {
        let ident = &f.ident;
        let cstr = std::ffi::CString::new(f.key.clone()).expect("field name contains NUL");
        let cname = Literal::c_string(&cstr);
        quote! {
            table.field(#cname, self.#ident);
        }
    });

    Ok(quote! {
        impl cosmos::userdata::PushLua for #name {
            unsafe fn push_lua(self, output: &mut cosmos::userdata::Output<'_>) -> i32 {
                let #mutable table = output.table(#n);
                #(#sets)*
                table.finish()
            }
        }
    })
}

// ---------------------------------------------------------------------------
// #[derive(PushLua)] 的枚举分支：按字段顺序生成 Lua 多返回值
//
// 每个分支按字段声明顺序压栈（字段需实现 PushLua），返回值个数 = 各字段
// 压栈个数之和。适合"一个 Rust 枚举对应多种 Lua 返回值形态"的场景：
// 功能模块只描述变体本身的字段，不再手写 match 分支 + lua_push*。
//
//   #[derive(cosmos::PushLua)]
//   enum Outcome {
//       Data(Vec<u8>),                      // 1 值
//       Closed(Nil, &'static str, Vec<u8>), // 3 值：nil, reason, partial
//   }
// ---------------------------------------------------------------------------

fn expand_push_lua_enum(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let Data::Enum(data) = &input.data else {
        return err(&input.ident, "#[derive(PushLua)] expected an enum");
    };
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let mut arms = Vec::with_capacity(data.variants.len());
    for variant in &data.variants {
        let vident = &variant.ident;
        let pattern = match &variant.fields {
            Fields::Unit => quote!(Self::#vident),
            Fields::Unnamed(fields) => {
                let bindings: Vec<_> = (0..fields.unnamed.len())
                    .map(|i| format_ident!("__field_{i}"))
                    .collect();
                quote!(Self::#vident(#(#bindings),*))
            }
            Fields::Named(fields) => {
                let bindings = fields.named.iter().enumerate().map(|(i, f)| {
                    let field = f.ident.as_ref().expect("named field");
                    let binding = format_ident!("__field_{i}");
                    quote!(#field: #binding)
                });
                quote!(Self::#vident { #(#bindings),* })
            }
        };
        let pushes = variant.fields.iter().enumerate().map(|(i, _)| {
            let binding = format_ident!("__field_{i}");
            quote!(+ cosmos::userdata::PushLua::push_lua(#binding, output))
        });
        // `0 + a.push_lua(output) + b.push_lua(output)`：操作数从左到右求值，保证压栈顺序
        arms.push(quote!(#pattern => 0i32 #(#pushes)*));
    }

    Ok(quote! {
        impl #impl_generics cosmos::userdata::PushLua for #name #ty_generics #where_clause {
            unsafe fn push_lua(self, output: &mut cosmos::userdata::Output<'_>) -> i32 {
                match self {
                    #(#arms),*
                }
            }
        }
    })
}
