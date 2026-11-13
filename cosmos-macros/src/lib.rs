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
use syn::{
    parse_macro_input, spanned::Spanned, FnArg, ImplItem, ItemImpl, PatType, PathArguments,
    ReturnType, Type,
};

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

    if sig.unsafety.is_some() {
        return err(&f.sig, "unsafe methods are not supported");
    }
    if sig.asyncness.is_some() {
        return err(&f.sig, "async methods are not supported");
    }
    if sig.abi.is_some() {
        return err(&f.sig, "extern methods are not supported");
    }
    if sig.variadic.is_some() {
        return err(&f.sig, "variadic methods are not supported");
    }

    // 静态方法（无 self 接收者）：不做 self 检查，参数从栈 1 开始；
    // 实例方法：首参是 Receiver，check 后参数从栈 2 开始
    let is_static = matches!(sig.inputs.first(), Some(FnArg::Typed(_)));

    let self_check = if is_static {
        quote!()
    } else {
        let receiver = match sig.inputs.first() {
            Some(FnArg::Receiver(r)) => r,
            _ => return err(&sig.inputs, "first argument must be &self or &mut self"),
        };
        if receiver.colon_token.is_some() {
            return err(&receiver.ty, "typed self is not supported");
        }
        let check_fn = match (&receiver.reference, &receiver.mutability) {
            (Some(_), None) => quote!(cosmos::userdata::check_shared),
            (Some(_), Some(_)) => quote!(cosmos::userdata::check),
            _ => return err(receiver, "only &self and &mut self receivers are supported"),
        };
        quote!(let this = #check_fn::<#ty>(L, 1);)
    };

    let mut arg_idents = Vec::new();
    let mut arg_conversions = Vec::new();
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
        check_arg_ty(arg_ty)?;
        let ident = format_ident!("arg{arg_n}");
        arg_n += 1;
        arg_idents.push(ident.clone());
        arg_conversions.push(quote! {
            let #ident = <#arg_ty as cosmos::userdata::FromLua>::from_lua(L, #stack_idx)?;
        });
        stack_idx += 1;
    }

    // 静态方法按关联函数调用 Type::method(...)，实例方法走 this.method(...)
    let call = if is_static {
        quote!(#ty::#method(#(#arg_idents),*))
    } else {
        quote!(this.#method(#(#arg_idents),*))
    };

    // 返回值：走 PushLua 直接压栈（trampoline 在 lua_resume 保护帧内，
    // userdata 等任意值零装箱）；Result<T, E> → Ok 压栈 / Err 转 lua_error
    let ret_conv = match &sig.output {
        ReturnType::Default => quote! {
            let ret = #call;
            Ok(<() as cosmos::userdata::PushLua>::push_lua(ret, L))
        },
        ReturnType::Type(_, ret_ty) => {
            if let Some((ok_ty, _)) = as_result(ret_ty) {
                quote! {
                    let ret = #call;
                    match ret {
                        ::core::result::Result::Ok(v) => {
                            Ok(<#ok_ty as cosmos::userdata::PushLua>::push_lua(v, L))
                        }
                        ::core::result::Result::Err(e) => {
                            Err(::std::string::ToString::to_string(&e))
                        }
                    }
                }
            } else {
                quote! {
                    let ret = #call;
                    Ok(<#ret_ty as cosmos::userdata::PushLua>::push_lua(ret, L))
                }
            }
        }
    };

    let tramp_ident = format_ident!("__cosmos_m_{}_{}", ty_label, method);
    // c 字面量（&'static CStr，NUL 结尾）：&str 字面量不保证 NUL，
    // 直接传给 lua_setfield 会读越界
    let c_name = {
        let cs = std::ffi::CString::new(method.to_string()).unwrap();
        Literal::c_string(&cs)
    };

    let tramp = quote! {
        #[allow(non_snake_case)]
        unsafe extern "C" fn #tramp_ident(
            L: *mut cosmos::ffi::lua_State,
        ) -> cosmos::ffi::c_int {
            cosmos::userdata::trampoline(L, |L| unsafe {
                #self_check
                #(#arg_conversions)*
                #ret_conv
            })
        }
    };

    Ok((c_name, tramp_ident, tramp))
}

fn check_arg_ty(ty: &Type) -> syn::Result<()> {
    if let Type::Path(p) = ty {
        if p.qself.is_none() {
            if let Some(seg) = p.path.segments.last() {
                if matches!(seg.ident.to_string().as_str(), "i64" | "f64" | "bool" | "String") {
                    return Ok(());
                }
            }
        }
    }
    err(ty, "unsupported argument type (supported: i64, f64, bool, String)")
}

/// 识别 Result<T, E>（路径末段为 Result 且带两个泛型参数）
fn as_result(ty: &Type) -> Option<(&Type, &Type)> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "Result" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    let mut types = args.args.iter().filter_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    });
    let ok = types.next()?;
    let err_ty = types.next()?;
    if types.next().is_some() {
        return None;
    }
    Some((ok, err_ty))
}
