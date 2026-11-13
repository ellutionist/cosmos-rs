//! #[cosmos::userdata]：把 impl 块里的方法自动桥接进 Lua metatable。
//!
//! 用法：
//! ```ignore
//! #[cosmos::userdata]
//! impl Counter {
//!     fn get(&self) -> i64 { ... }
//!     fn inc(&mut self, by: i64) -> i64 { ... }
//! }
//!
//! // 自定义 Lua require 路径（默认是类型名，如 require("cosmos.Counter")）
//! #[cosmos::userdata(name = "http.client")]
//! impl HttpClient { ... }
//! ```
//!
//! 每个方法生成一个 extern "C" trampoline（参数转换 / panic 捕获 /
//! 返回值压栈，见 cosmos::userdata::trampoline），并生成
//! `impl cosmos::userdata::UserData` 把它们注册进 MethodTable。
//! `__gc` 不需要宏处理：运行时用 `ptr::drop_in_place` 统一生成，
//! 对没有 Drop 的类型同样是合法空操作。

use proc_macro::TokenStream;
use proc_macro2::Literal;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, spanned::Spanned, Data, DataStruct, DeriveInput, Field, Fields, FnArg,
    ImplItem, ItemImpl, LitByteStr, LitStr, PatType, ReturnType, Type,
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
    let lua_name = syn::LitStr::new(
        name_override.as_deref().unwrap_or(&ty_label),
        ty.span(),
    );

    let mut trampolines = Vec::new();
    let mut registrations = Vec::new();
    for item in &imp.items {
        match item {
            ImplItem::Fn(f) => match gen_method(ty, &ty_label, f) {
                Ok((c_name, tramp_ident, tramp)) => {
                    registrations.push(quote! { methods.add(#c_name, #tramp_ident); });
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

/// `#[cosmos::userdata(name = "http.client")]`：Lua require 路径覆盖，
/// 默认是类型短名（require("cosmos.Counter")）
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

/// 生成单个方法的 trampoline。返回 (方法名 c 字面量, trampoline 项)
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
                    quote!(let this = cosmos::userdata::check_shared::<#ty>(L, 1);)
                }
                (Some(_), Some(_)) => {
                    quote!(let mut this = cosmos::userdata::check::<#ty>(L, 1);)
                }
                _ => return err(r, "only &self and &mut self receivers are supported"),
            };
            (false, binding)
        }
        Some(FnArg::Typed(_)) => (true, quote!()),
        // 无参静态方法（如 TlsContext.new()）：参数从栈 1 开始，但没有要转换的实参
        None => (true, quote!()),
    };

    let mut arg_idents = Vec::new();
    let mut arg_conversions_sync = Vec::new();
    let mut arg_conversions_async = Vec::new();
    let mut stack_idx: i32 = if is_static { 1 } else { 2 };
    let mut arg_n: usize = 0;
    let mut inputs = sig.inputs.iter();
    if !is_static {
        inputs.next(); // 跳过 receiver
    }
    for arg in inputs {
        let FnArg::Typed(PatType { ty: arg_ty, .. }) = arg else {
            return err(arg, "self must be the first argument");
        };
        if is_async {
            if matches!(arg_ty.as_ref(), Type::Reference(_)) {
                return err(
                    arg_ty,
                    "async methods cannot borrow Lua arguments across yield; \
                     use owned types (e.g. String instead of &str)",
                );
            }
        }
        let ident = format_ident!("arg{arg_n}");
        arg_n += 1;
        arg_idents.push(ident.clone());
        arg_conversions_sync.push(quote! {
            let #ident = <#arg_ty as cosmos::userdata::FromLua>::from_lua(L, #stack_idx)?;
        });
        arg_conversions_async.push(quote! {
            let #ident = match <#arg_ty as cosmos::userdata::FromLua>::from_lua(L, #stack_idx) {
                ::core::result::Result::Ok(v) => v,
                ::core::result::Result::Err(e) => return cosmos::userdata::raise(L, &e),
            };
        });
        stack_idx += 1;
    }

    // 静态方法按关联函数调用 Type::method(...)，实例方法走 this.method(...)
    let call = if is_static {
        quote!(#ty::#method(#(#arg_idents),*))
    } else {
        quote!(this.#method(#(#arg_idents),*))
    };

    let tramp_ident = format_ident!("__cosmos_m_{}_{}", ty_label, method);
    // c 字面量（&'static CStr，NUL 结尾）：&str 字面量不保证 NUL，
    // 直接传给 lua_setfield 会读越界
    let c_name = {
        let cs = std::ffi::CString::new(method.to_string()).unwrap();
        Literal::c_string(&cs)
    };

    let tramp = if is_async {
        // async 方法：守卫（RefMut/Ref）被 move 进外层 future 持有，
        // 整个 await 期间 RefCell borrow flag 保持"已借"，其他方法调用报 lua_error。
        // yield_with 必须是尾表达式（5.1 语义：lua_yield 的 -1 要作为返回值交出去）。
        // Result<T, E> 由 IntoOutput 的 blanket impl 统一处理（Ok 走值 / Err 走 NilErr）
        let async_body = match &sig.output {
            ReturnType::Default => {
                quote!(<() as cosmos::userdata::IntoOutput>::into_output(#call.await))
            }
            ReturnType::Type(_, ret_ty) => {
                quote!(<#ret_ty as cosmos::userdata::IntoOutput>::into_output(#call.await))
            }
        };
        quote! {
            #[allow(non_snake_case)]
            unsafe extern "C" fn #tramp_ident(
                L: *mut cosmos::ffi::lua_State,
            ) -> cosmos::ffi::c_int {
                unsafe {
                    #self_binding
                    #(#arg_conversions_async)*
                    cosmos::coroutine::yield_with(
                        L,
                        Box::pin(async move { #async_body }),
                    )
                }
            }
        }
    } else {
        // 同步方法：走 PushLua 直接压栈（trampoline 在 lua_resume 保护帧内）。
        // Result<T, E> 由 PushLua 的 blanket impl 统一处理（Ok 压值 / Err 压 nil+err）
        let ret_conv = match &sig.output {
            ReturnType::Default => quote! {
                let ret = #call;
                Ok(<() as cosmos::userdata::PushLua>::push_lua(ret, L))
            },
            ReturnType::Type(_, ret_ty) => quote! {
                let ret = #call;
                Ok(<#ret_ty as cosmos::userdata::PushLua>::push_lua(ret, L))
            },
        };
        quote! {
            #[allow(non_snake_case)]
            unsafe extern "C" fn #tramp_ident(
                L: *mut cosmos::ffi::lua_State,
            ) -> cosmos::ffi::c_int {
                cosmos::userdata::trampoline(L, |L| unsafe {
                    #self_binding
                    #(#arg_conversions_sync)*
                    #ret_conv
                })
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
// 未知键忽略；非字符串键跳过（绝不对键做会改写键值的转换，避免破坏 lua_next）。
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
                Err(meta.error("unknown struct-level lua attribute (expected default / default_fn)"))
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
    let name = &input.ident;
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
                // 传绝对下标：嵌套 from_lua 可能先 push 再按 idx 取表（如 Headers），
                // 负下标此时会指向新压入的值
                let value_idx = cosmos::ffi::lua_gettop(L);
                #s = ::core::option::Option::Some(
                    <#ty as cosmos::userdata::FromLua>::from_lua(L, value_idx)?,
                );
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
            cosmos::ffi::lua_pushnil(L);
            while cosmos::ffi::lua_next(L, idx) != 0 {
                if cosmos::ffi::lua_type(L, -2) == cosmos::ffi::LUA_TSTRING {
                    let mut key_len: cosmos::ffi::size_t = 0;
                    let key_ptr = cosmos::ffi::lua_tolstring(L, -2, &mut key_len);
                    let key: &[u8] =
                        ::core::slice::from_raw_parts(key_ptr as *const u8, key_len);
                    #(#checks)*
                }
                cosmos::ffi::lua_settop(L, -2);
            }
        }
    };

    Ok(quote! {
        impl cosmos::userdata::FromLua for #name {
            unsafe fn from_lua(
                L: *mut cosmos::ffi::lua_State,
                idx: cosmos::ffi::c_int,
            ) -> ::core::result::Result<Self, ::std::string::String> {
                if cosmos::ffi::lua_type(L, idx) != cosmos::ffi::LUA_TTABLE {
                    return ::core::result::Result::Err(::std::format!(
                        "argument {}: expected table", idx
                    ));
                }
                #(#decls)*
                #container_init
                #scan
                Ok(Self { #(#values),* })
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
    let name = &input.ident;
    let fields = lua_fields(input)?;
    let n = fields.len() as i32;

    let sets = fields.iter().map(|f| {
        let ident = &f.ident;
        let ty = &f.ty;
        let cstr = std::ffi::CString::new(f.key.clone()).expect("field name contains NUL");
        let cname = Literal::c_string(&cstr);
        quote! {
            <#ty as cosmos::userdata::PushLua>::push_lua(self.#ident, L);
            cosmos::ffi::lua_setfield(L, -2, #cname.as_ptr());
        }
    });

    Ok(quote! {
        impl cosmos::userdata::PushLua for #name {
            fn push_lua(self, L: *mut cosmos::ffi::lua_State) -> cosmos::ffi::c_int {
                unsafe {
                    cosmos::ffi::lua_createtable(L, 0, #n as cosmos::ffi::c_int);
                    #(#sets)*
                }
                1
            }
        }
    })
}

// ---------------------------------------------------------------------------
// #[derive(IntoOutput)]：把一个 Rust 枚举派发成 Lua 的多返回值形态
//
// 每个分支按字段声明顺序压栈（字段需实现 PushLua），返回值个数 = 各字段
// 压栈个数之和。适合"一个 Rust 枚举对应多种 Lua 返回值形态"的场景：
// 功能模块只描述变体本身的字段，不再手写 match 分支 + lua_push*。
//
//   #[derive(cosmos::IntoOutput)]
//   enum Outcome {
//       Data(Vec<u8>),                      // 1 值
//       Closed(Nil, &'static str, Vec<u8>), // 3 值：nil, reason, partial
//   }
// ---------------------------------------------------------------------------

#[proc_macro_derive(IntoOutput)]
pub fn derive_into_output(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_into_output(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand_into_output(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let Data::Enum(data) = &input.data else {
        return err(&input.ident, "#[derive(IntoOutput)] only supports enums");
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
                let bindings: Vec<_> = fields
                    .named
                    .iter()
                    .map(|f| f.ident.clone().expect("named field"))
                    .collect();
                quote!(Self::#vident { #(#bindings),* })
            }
        };
        let pushes = variant.fields.iter().enumerate().map(|(i, f)| {
            let binding = match &f.ident {
                Some(ident) => ident.clone(),
                None => format_ident!("__field_{i}"),
            };
            quote!(+ cosmos::userdata::PushLua::push_lua(#binding, L))
        });
        // `0 + a.push_lua(L) + b.push_lua(L)`：操作数从左到右求值，保证压栈顺序
        arms.push(quote!(#pattern => 0i32 #(#pushes)*));
    }

    Ok(quote! {
        impl #impl_generics cosmos::userdata::IntoOutput for #name #ty_generics #where_clause {
            fn into_output(self) -> cosmos::coroutine::AsyncOutput {
                cosmos::coroutine::AsyncOutput::Custom(::std::boxed::Box::new(move |L| {
                    match self {
                        #(#arms),*
                    }
                }))
            }
        }
    })
}
