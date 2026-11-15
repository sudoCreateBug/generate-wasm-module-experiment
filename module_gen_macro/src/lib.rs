// TODO: investigate what spans are.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Token, parse_macro_input};
use wit_parser::{
    Function, FunctionKind, Interface, PackageId, Resolve, Type, TypeDefKind, WorldItem,
};

const ANNOTATION_PREFIX: &str = "@dist_gen";
const ERROR_TYPE: &str = "invocation-error";
const PACKAGE_SUFFIX: &str = "-wrapped";
const WRAPPER_WORLD: &str = "wrapper";

#[proc_macro]
pub fn wrap_in_result(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as MacroArgs);
    match expand_wrap(&args) {
        Ok(tokens) => tokens.into(),
        Err(message) => quote! { compile_error!(#message); }.into(),
    }
}

struct MacroArgs {
    path: String,
    world: String,
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let (mut path, mut world) = (None, None);
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let value: LitStr = input.parse()?;
            match key.to_string().as_str() {
                "path" => path = Some(value.value()),
                "world" => world = Some(value.value()),
                _ => return Err(input.error("expected `path` or `world`")),
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        let missing = |name| input.error(format!("missing `{name} = \"...\"`"));
        Ok(MacroArgs {
            path: path.ok_or_else(|| missing("path"))?,
            world: world.ok_or_else(|| missing("world"))?,
        })
    }
}

fn expand_wrap(args: &MacroArgs) -> Result<TokenStream2, String> {
    let wit_dir = manifest_relative(&args.path);
    let (resolve, package_id) = load_wit(&wit_dir)?;
    let world_id = resolve
        .select_world(&[package_id], Some(&args.world))
        .map_err(|e| format!("{e:#}"))?;
    let world = &resolve.worlds[world_id];
    let package = &resolve.packages[package_id];

    if package.name.version.is_some() {
        return Err("versioned packages are not supported yet".into());
    }
    let namespace = &package.name.namespace;
    let wrapped_package = format!("{}{PACKAGE_SUFFIX}", package.name.name);

    let mut wit = format!("package {namespace}:{wrapped_package};\n");
    let mut world_wit = format!("\nworld {WRAPPER_WORLD} {{\n");
    let mut impls = TokenStream2::new();

    for item in world.exports.values() {
        let interface_id = match item {
            WorldItem::Interface { id: interface_id, .. } => interface_id,
            _ => {
                return Err("This experiment expect a world of interfaces, no other types supported yet".into());
            }
        };
        let interface = &resolve.interfaces[*interface_id];
        let iface_name = match &interface.name {
            Some(name) => name,
            None => {
                return Err(format!(
                    "world `{}` exports an anonymous interface; only named interfaces can be wrapped",
                    world.name
                ));
            }
        };
        if interface.package != Some(package_id) {
            return Err(format!(
                "interface `{iface_name}` is from another package; only interfaces of `{}` \
                 can be wrapped",
                package.name
            ));
        }
        let original_id = format!("{}/{iface_name}", package.name);

        wit.push_str(&wrapped_interface_wit(&resolve, interface, iface_name)?);
        writeln!(world_wit, "    import {original_id};").unwrap();
        writeln!(world_wit, "    export {iface_name};").unwrap();

        impls.extend(wrapped_interface_impl(
            &resolve,
            interface,
            iface_name,
            namespace,
            &package.name.name,
            &wrapped_package,
        )?);
    }
    world_wit.push_str("}\n");
    wit.push_str(&world_wit);

    let path = wit_dir.display().to_string();
    let world = format!("{namespace}:{wrapped_package}/{WRAPPER_WORLD}");

    Ok(quote! {
        /// The WIT this wrapper was generated from, for inspection.
        pub const GENERATED_WIT: &str = #wit;

        wit_bindgen::generate!({
            path: #path,
            inline: #wit,
            world: #world,
            // Also generate bindings for the imported original interface.
            generate_all,
        });

        struct Wrapper;

        #impls

        export!(Wrapper);
    })
}

/// WIT text of the wrapped version of `interface`.
fn wrapped_interface_wit(
    resolve: &Resolve,
    interface: &Interface,
    name: &str,
) -> Result<String, String> {
    let mut wit = format!("\ninterface {name} {{\n");
    writeln!(wit, "    variant {ERROR_TYPE} {{\n        failed(string),\n    }}\n").unwrap();

    for func in interface.functions.values() {
        check_supported(func)?;
        let params = func
            .params
            .iter()
            .map(|p| Ok(format!("{}: {}", p.name, wit_type(resolve, &p.ty, func)?)))
            .collect::<Result<Vec<_>, String>>()?
            .join(", ");
        let ok = match &func.result {
            Some(ty) => wit_type(resolve, ty, func)?,
            None => "_".into(),
        };
        writeln!(
            wit,
            "    {}: func({params}) -> result<{ok}, {ERROR_TYPE}>;",
            func.name
        )
        .unwrap();
    }
    wit.push_str("}\n");
    Ok(wit)
}

/// `impl Guest for Wrapper` forwarding every function of the wrapped
/// interface to the original import.
fn wrapped_interface_impl(
    resolve: &Resolve,
    interface: &Interface,
    iface_name: &str,
    namespace: &str,
    package: &str,
    wrapped_package: &str,
) -> Result<TokenStream2, String> {
    let namespace = format_ident!("{}", rust_ident(namespace));
    let package = format_ident!("{}", rust_ident(package));
    let wrapped_package = format_ident!("{}", rust_ident(wrapped_package));
    let iface = format_ident!("{}", rust_ident(iface_name));
    let error = format_ident!("{}", upper_camel(ERROR_TYPE));
    let wrapped = quote! { exports::#namespace::#wrapped_package::#iface };

    let mut methods = TokenStream2::new();
    for func in interface.functions.values() {
        let name = format_ident!("{}", rust_ident(&func.name));
        let mut params = Vec::new();
        let mut args = Vec::new();
        for param in &func.params {
            let ident = format_ident!("{}", rust_ident(&param.name));
            let ty = rust_type(resolve, &param.ty, func)?;
            params.push(quote! { #ident: #ty });
            // Imports borrow strings and lists; exports receive them owned.
            args.push(if passed_by_reference(resolve, &param.ty) {
                quote! { &#ident }
            } else {
                quote! { #ident }
            });
        }
        let ok = match &func.result {
            Some(ty) => rust_type(resolve, ty, func)?,
            None => quote! { () },
        };
        methods.extend(quote! {
            fn #name(#(#params),*) -> Result<#ok, #wrapped::#error> {
                Ok(#namespace::#package::#iface::#name(#(#args),*))
            }
        });
    }

    Ok(quote! {
        impl #wrapped::Guest for Wrapper {
            #methods
        }
    })
}

fn check_supported(func: &Function) -> Result<(), String> {
    match func.kind {
        FunctionKind::Freestanding => Ok(()),
        _ => Err("Unsupported function encountered".into()),
    }
}

/// Error for types this experiment cannot wrap yet.
fn unsupported(resolve: &Resolve, ty: &Type, func: &Function) -> String {
    let what = match ty {
        Type::Id(id) => {
            let def = &resolve.types[*id];
            match &def.name {
                Some(name) => format!("{} `{name}`", def.kind.as_str()),
                None => def.kind.as_str().to_string(),
            }
        }
        _ => format!("{ty:?}"),
    };
    format!(
        "`{}` uses a {what}, which is not supported yet \
         (supported: primitives, string, list<T>, option<T>)",
        func.name
    )
}

/// Renders a type as WIT source.
fn wit_type(resolve: &Resolve, ty: &Type, func: &Function) -> Result<String, String> {
    Ok(match ty {
        Type::Bool => "bool".into(),
        Type::U8 => "u8".into(),
        Type::U16 => "u16".into(),
        Type::U32 => "u32".into(),
        Type::U64 => "u64".into(),
        Type::S8 => "s8".into(),
        Type::S16 => "s16".into(),
        Type::S32 => "s32".into(),
        Type::S64 => "s64".into(),
        Type::F32 => "f32".into(),
        Type::F64 => "f64".into(),
        Type::Char => "char".into(),
        Type::String => "string".into(),
        Type::ErrorContext => return Err(unsupported(resolve, ty, func)),
        Type::Id(id) => match &resolve.types[*id] {
            def if def.name.is_some() => return Err(unsupported(resolve, ty, func)),
            def => match &def.kind {
                TypeDefKind::List(t) => format!("list<{}>", wit_type(resolve, t, func)?),
                TypeDefKind::Option(t) => format!("option<{}>", wit_type(resolve, t, func)?),
                _ => return Err(unsupported(resolve, ty, func)),
            },
        },
    })
}

/// Renders a type as the owned Rust type wit-bindgen uses for exports.
fn rust_type(resolve: &Resolve, ty: &Type, func: &Function) -> Result<TokenStream2, String> {
    Ok(match ty {
        Type::Bool => quote! { bool },
        Type::U8 => quote! { u8 },
        Type::U16 => quote! { u16 },
        Type::U32 => quote! { u32 },
        Type::U64 => quote! { u64 },
        Type::S8 => quote! { i8 },
        Type::S16 => quote! { i16 },
        Type::S32 => quote! { i32 },
        Type::S64 => quote! { i64 },
        Type::F32 => quote! { f32 },
        Type::F64 => quote! { f64 },
        Type::Char => quote! { char },
        Type::String => quote! { String },
        Type::ErrorContext => return Err(unsupported(resolve, ty, func)),
        Type::Id(id) => match &resolve.types[*id] {
            def if def.name.is_some() => return Err(unsupported(resolve, ty, func)),
            def => match &def.kind {
                TypeDefKind::List(t) => {
                    let t = rust_type(resolve, t, func)?;
                    quote! { Vec<#t> }
                }
                TypeDefKind::Option(t) => {
                    // An import taking `option<string>` wants `Option<&str>`;
                    // keep this experiment to options of by-value types.
                    if passed_by_reference(resolve, t) {
                        return Err(unsupported(resolve, ty, func));
                    }
                    let t = rust_type(resolve, t, func)?;
                    quote! { Option<#t> }
                }
                _ => return Err(unsupported(resolve, ty, func)),
            },
        },
    })
}

/// Whether wit-bindgen's imports take this type by reference (`&str`, `&[T]`).
fn passed_by_reference(resolve: &Resolve, ty: &Type) -> bool {
    match ty {
        Type::String => true,
        Type::Id(id) => matches!(resolve.types[*id].kind, TypeDefKind::List(_)),
        _ => false,
    }
}

#[proc_macro]
pub fn dist_gen_str(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as DistGenArgs);
    match expand_dist_gen_str(&args) {
        Ok(tokens) => tokens.into(),
        Err(message) => quote! { compile_error!(#message) }.into(),
    }
}

struct DistGenArgs {
    path: String,
    interface: String,
    function: String,
}

impl Parse for DistGenArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let path: LitStr = input.parse()?;
        input.parse::<Token![,]>()?;
        let interface: LitStr = input.parse()?;
        input.parse::<Token![,]>()?;
        let function: LitStr = input.parse()?;
        input.parse::<Option<Token![,]>>()?;
        Ok(DistGenArgs {
            path: path.value(),
            interface: interface.value(),
            function: function.value(),
        })
    }
}

fn expand_dist_gen_str(args: &DistGenArgs) -> Result<TokenStream2, String> {
    let wit_dir = manifest_relative(&args.path);
    let (resolve, package_id) = load_wit(&wit_dir)?;
    let package = &resolve.packages[package_id];

    let interface = package
        .interfaces
        .get(&args.interface)
        .map(|id| &resolve.interfaces[*id])
        .ok_or_else(|| format!("no interface `{}` in `{}`", args.interface, package.name))?;
    let func = interface
        .functions
        .get(&args.function)
        .ok_or_else(|| format!("no function `{}` in `{}`", args.function, args.interface))?;

    let docs = func.docs.contents.as_deref().unwrap_or_default();
    let annotations = parse_annotations(docs)
        .map_err(|e| format!("{e} in doc comment of `{}`", func.name))?;
    let contents = match annotations.first() {
        Some(contents) => contents,
        None => {
            return Err(format!(
                "`{}.{}` has no `{ANNOTATION_PREFIX}(...)` annotation",
                args.interface, args.function
            ));
        }
    };

    let value = syn::parse_str::<LitStr>(contents)
        .map(|lit| lit.value())
        .unwrap_or_else(|_| contents.clone());
    let tracked = tracked_files(&wit_dir);

    // The `include_str!`s make cargo rerun the macro when the WIT changes.
    Ok(quote! {{
        #(const _: &str = include_str!(#tracked);)*
        #value
    }})
}

fn parse_annotations(text: &str) -> Result<Vec<String>, String> {
    let opener = format!("{ANNOTATION_PREFIX}(");
    let mut found = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find(&opener) {
        let body = &rest[start + opener.len()..];
        let (mut depth, mut in_string, mut escaped, mut end) = (1, false, false, None);
        for (i, c) in body.char_indices() {
            match c {
                _ if escaped => escaped = false,
                '\\' if in_string => escaped = true,
                '"' => in_string = !in_string,
                '(' if !in_string => depth += 1,
                ')' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = match end {
            Some(end) => end,
            None => {
                let problem = if in_string {
                    "unterminated string"
                } else {
                    "unbalanced parentheses"
                };
                return Err(format!("{problem} in `{ANNOTATION_PREFIX}` annotation"));
            }
        };
        found.push(body[..end].trim().to_string());
        rest = &body[end + 1..];
        if rest.starts_with(')') {
            return Err(format!(
                "unbalanced parentheses (extra `)`) in `{ANNOTATION_PREFIX}` annotation"
            ));
        }
    }
    Ok(found)
}

// Shared helpers

fn manifest_relative(path: &str) -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let path = PathBuf::from(manifest_dir).join(path);
    path.canonicalize().unwrap_or(path)
}

fn load_wit(path: &Path) -> Result<(Resolve, PackageId), String> {
    let mut resolve = Resolve::default();
    let (package, _) = resolve
        .push_path(path)
        .map_err(|e| format!("failed to parse WIT: {e:?}"))?;
    Ok((resolve, package))
}

fn tracked_files(path: &Path) -> Vec<String> {
    let mut resolve = Resolve::default();
    match resolve.push_path(path) {
        Ok((_, sources)) => sources.paths().map(|p| p.display().to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

/// kebab-case WIT name to the snake_case identifier wit-bindgen uses.
fn rust_ident(name: &str) -> String {
    let ident = name.replace('-', "_");
    // wit-bindgen appends `_` to Rust keywords (`to_rust_ident`).
    const KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
        "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final",
        "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
    ];
    if KEYWORDS.contains(&ident.as_str()) {
        format!("{ident}_")
    } else {
        ident
    }
}

/// kebab-case WIT name to the UpperCamelCase type name wit-bindgen uses.
fn upper_camel(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect()
}
