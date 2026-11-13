//! #[cosmos::userdata]：把 impl 块里的方法自动桥接进 Lua metatable。
//!
//! 用法：
//! ```ignore
//! #[cosmos::userdata]
//! impl Counter {
//!     fn get(&self) -> i64 { ... }
//!     fn inc(&mut self, by: i64) -> i64 { ... }
//! }
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
use syn::{parse_macro_input, spanned::Spanned, FnArg, ImplItem, ItemImpl, PatType, ReturnType, Type};

#[proc_macro_attribute]
pub fn userdata(_attr: TokenStream, item: TokenStream) -> TokenStream {
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
    let lua_name = syn::LitStr::new(&ty_label, ty.span());

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
        None => return err(&sig.inputs, "method needs a receiver or arguments"),
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
