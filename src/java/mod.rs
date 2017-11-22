//! Functions for converting Rust types to Java types.

use common::{self, Outputs, is_user_data_arg, is_result_arg, is_array_arg, parse_attr,
             check_no_mangle, retrieve_docstring};
use inflector::Inflector;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use syntax::ast;
use syntax::abi::Abi;
use syntax::print::pprust;
use syntax::codemap;

use Error;
use Level;

mod jni;

pub struct LangJava;

impl common::Lang for LangJava {
    /// Convert a Rust function declaration into Java.
    fn parse_fn(item: &ast::Item, outputs: &mut Outputs) -> Result<(), Error> {
        let (no_mangle, docs) = parse_attr(&item.attrs, check_no_mangle, |attr| {
            retrieve_docstring(attr, "")
        });
        // If it's not #[no_mangle] then it can't be called from C.
        if !no_mangle {
            return Ok(());
        }

        let name = item.ident.name.as_str();

        if let ast::ItemKind::Fn(ref fn_decl, _, _, abi, ref generics, _) = item.node {
            match abi {
                // If it doesn't have a C ABI it can't be called from C.
                Abi::C | Abi::Cdecl | Abi::Stdcall | Abi::Fastcall | Abi::System => {}
                _ => return Ok(()),
            }

            if generics.is_parameterized() {
                return Err(Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: "cheddar can not handle parameterized extern functions".into(),
                });
            }

            transform_native_fn(&*fn_decl, &docs, &format!("{}", name), outputs)?;

            Ok(())
        } else {
            Err(Error {
                level: Level::Bug,
                span: Some(item.span),
                message: "`parse_fn` called on wrong `Item_`".into(),
            })
        }
    }

    /// Convert a Rust struct into a Java class.
    fn parse_struct(item: &ast::Item, outputs: &mut Outputs) -> Result<(), Error> {
        let (repr_c, docs) = parse_attr(&item.attrs, common::check_repr_c, |attr| {
            retrieve_docstring(attr, "")
        });
        // If it's not #[repr(C)] then it can't be called from C.
        if !repr_c {
            return Ok(());
        }

        let mut buffer = String::new();
        buffer.push_str(&docs);

        let name = item.ident.name.as_str().to_class_case();
        buffer.push_str(&format!("public class {}", name));

        if let ast::ItemKind::Struct(ref variants, ref generics) = item.node {
            if generics.is_parameterized() {
                return Err(Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: "cheddar can not handle parameterized `#[repr(C)]` structs".into(),
                });
            }

            if variants.is_struct() {
                buffer.push_str(" {\n");

                for field in variants.fields() {
                    let (_, docs) = parse_attr(
                        &field.attrs,
                        |_| true,
                        |attr| retrieve_docstring(attr, "\t"),
                    );
                    buffer.push_str(&docs);

                    let name = match field.ident {
                        Some(name) => name.name.as_str().to_camel_case(),
                        None => unreachable!("a tuple struct snuck through"),
                    };
                    let ty = rust_to_java(&*field.ty)?.unwrap_or_default();
                    buffer.push_str(&format!("\tpublic {} {};\n", ty, name));
                }

                buffer.push_str("}");
            } else if variants.is_tuple() && variants.fields().len() == 1 {
                // #[repr(C)] pub struct Foo(Bar);  =>  typedef struct Foo Foo;
            } else {
                return Err(Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: "cheddar can not handle unit or tuple `#[repr(C)]` structs with >1 members"
                        .into(),
                });
            }
        } else {
            return Err(Error {
                level: Level::Bug,
                span: Some(item.span),
                message: "`parse_struct` called on wrong `Item_`".into(),
            });
        }

        buffer.push_str(";\n\n");

        outputs.insert(From::from(format!("{}.java", name)), buffer);

        Ok(())
    }

    fn finalise_output(outputs: &mut Outputs) -> Result<(), Error> {
        match outputs.get_mut(&PathBuf::from("NativeBindings.java")) {
            Some(funcs) => {
                *funcs = format!("public class NativeBindings {{\n{}\n}}", funcs);
                Ok(())
            }
            None => Err(Error {
                level: Level::Error,
                span: None,
                message: "no native bindings generated?".to_owned(),
            }),
        }
    }
}

/// Get the Java interface name for the callback based on its types
pub fn callback_name(inputs: &[ast::Arg]) -> Result<String, Error> {
    let mut basename = String::from("Callback");

    let mut inputs = inputs.iter().peekable();

    while let Some(arg) = inputs.next() {
        if is_user_data_arg(arg) || is_result_arg(arg) {
            // Skip user_data args
            continue;
        }

        let mut arg_type = anon_rust_to_java(&*arg.ty)?
            .map(|s| s.to_class_case())
            .unwrap_or_default();

        if is_array_arg(&arg, inputs.peek().cloned()) {
            inputs.next();
            arg_type.push_str("Array");
        }

        basename.push_str(&arg_type);
    }

    Ok(basename)
}

/// Transform a Rust FFI function into a Java native function
pub fn transform_native_fn(
    fn_decl: &ast::FnDecl,
    docs: &str,
    name: &str,
    outputs: &mut Outputs,
) -> Result<(), Error> {
    let mut args_str = Vec::new();

    let mut fn_args = fn_decl
        .inputs
        .iter()
        .filter(|arg| !is_user_data_arg(arg))
        .peekable();

    while let Some(arg) = fn_args.next() {
        let arg_name = pprust::pat_to_string(&*arg.pat);

        // Generate function arguments
        let mut java_type = rust_to_java(&arg.ty)?.unwrap_or_default();

        if is_array_arg(&arg, fn_args.peek().cloned()) {
            // This is an array, so add it to the type description
            java_type.push_str("[]");

            // Skip the length args - e.g. for a case of `ptr: *const u8, ptr_len: usize` we're going to skip the `len` part.
            fn_args.next();
        }

        args_str.push(format!("{} {}", java_type, arg_name.to_camel_case()));

        // Generate a callback class - if it wasn't generated already
        if let ast::TyKind::BareFn(ref bare_fn) = arg.ty.node {
            let cb_class = callback_name(&*bare_fn.decl.inputs)?;
            let cb_file = PathBuf::from(format!("{}.java", cb_class));

            if let None = outputs.get(&cb_file) {
                eprintln!("Generating CB {}", cb_class);

                println!("{}\n", jni::generate_jni_callback(bare_fn, &cb_class));

                let cb_output = transform_callback(&*arg.ty, &cb_class)?.unwrap_or_default();
                let _ = outputs.insert(cb_file, cb_output);
            }
        }
    }

    let output_type = &fn_decl.output;
    let return_type = match *output_type {
        ast::FunctionRetTy::Ty(ref ty) if ty.node == ast::TyKind::Never => {
            return Err(Error {
                level: Level::Error,
                span: Some(ty.span),
                message: "panics across a C boundary are naughty!".into(),
            });
        }
        ast::FunctionRetTy::Default(..) => String::from("public static native void"),
        ast::FunctionRetTy::Ty(ref ty) => rust_to_java(&*ty)?.unwrap_or_default(),
    };

    let java_name = name.to_camel_case();
    let func_decl = format!(
        "{} {}({})",
        return_type,
        &java_name,
        args_str.as_slice().join(", ")
    );

    let mut buffer = String::new();
    buffer.push_str("/**\n");
    buffer.push_str(&docs.replace("///", " *"));
    buffer.push_str(" */\n");
    buffer.push_str(&func_decl);
    buffer.push_str(";\n\n");

    match outputs.entry(From::from("NativeBindings.java")) {
        Entry::Occupied(o) => o.into_mut().push_str(&buffer),
        Entry::Vacant(v) => {
            let _ = v.insert(buffer);
        }
    }

    println!(
        "{}\n",
        jni::generate_jni_function(fn_decl.inputs.clone(), name, &java_name)
    );

    Ok(())
}

/// Turn a Rust callback function type into a Java interface.
pub fn transform_callback<S: AsRef<str>>(
    ty: &ast::Ty,
    class_name: S,
) -> Result<Option<String>, Error> {
    match ty.node {
        ast::TyKind::BareFn(ref bare_fn) => Ok(Some(format!(
            "public interface {name} {{\n    public call({types});\n}}\n",
            name = class_name.as_ref(),
            types = try_some!(callback_to_java(bare_fn, ty.span)),
        ))),
        // All other types just have a name associated with them.
        _ => Err(Error {
            level: Level::Error,
            span: Some(ty.span),
            message: "Invalid callback type".into(),
        }),
    }
}

/// Transform a Rust FFI callback into Java function signature
fn callback_to_java(
    fn_ty: &ast::BareFnTy,
    fn_span: codemap::Span,
) -> Result<Option<String>, Error> {
    match fn_ty.abi {
        // If it doesn't have a C ABI it can't be called from C.
        Abi::C | Abi::Cdecl | Abi::Stdcall | Abi::Fastcall | Abi::System => {}
        _ => return Ok(None),
    }

    if !fn_ty.lifetimes.is_empty() {
        return Err(Error {
            level: Level::Error,
            span: Some(fn_span),
            message: "cheddar can not handle lifetimes".into(),
        });
    }

    let fn_decl: &ast::FnDecl = &*fn_ty.decl;
    let mut args = Vec::new();

    let mut args_iter = fn_decl
        .inputs
        .iter()
        .filter(|arg| !is_user_data_arg(arg))
        .peekable();

    while let Some(arg) = args_iter.next() {
        let arg_name = pprust::pat_to_string(&*arg.pat);
        let mut java_type = try_some!(rust_to_java(&*arg.ty));

        if is_array_arg(&arg, args_iter.peek().cloned()) {
            // Detect array ptrs: skip the length args and add array to the type sig
            java_type.push_str("[]");
            args_iter.next();
        }

        args.push(format!("{} {}", java_type, arg_name.to_camel_case()));
    }

    Ok(Some(args.join(", ")))
}

/// Converts a callback function argument into a Java interface name
fn callback_arg_to_java(
    fn_ty: &ast::BareFnTy,
    fn_span: codemap::Span,
) -> Result<Option<String>, Error> {
    match fn_ty.abi {
        // If it doesn't have a C ABI it can't be called from C.
        Abi::C | Abi::Cdecl | Abi::Stdcall | Abi::Fastcall | Abi::System => {}
        _ => return Ok(None),
    }

    if !fn_ty.lifetimes.is_empty() {
        return Err(Error {
            level: Level::Error,
            span: Some(fn_span),
            message: "can not handle lifetimes".into(),
        });
    }

    Ok(Some(callback_name(&*fn_ty.decl.inputs)?))
}

/// Turn a Rust type with an associated name or type into a C type.
pub fn rust_to_java(ty: &ast::Ty) -> Result<Option<String>, Error> {
    match ty.node {
        // This is a callback ref taken as a function argument
        ast::TyKind::BareFn(ref bare_fn) => callback_arg_to_java(bare_fn, ty.span),

        // All other types just have a name associated with them.
        _ => anon_rust_to_java(ty),
    }
}

/// Turn a Rust type into a Java type signature.
fn anon_rust_to_java(ty: &ast::Ty) -> Result<Option<String>, Error> {
    match ty.node {
        // Function pointers should not be in this function.
        ast::TyKind::BareFn(..) => Err(Error {
            level: Level::Error,
            span: Some(ty.span),
            message: "C function pointers must have a name or function declaration associated with them"
                .into(),
        }),

        // Standard pointers.
        ast::TyKind::Ptr(ref ptr) => {
            // Detect strings, which are *const c_char or *mut c_char
            if pprust::ty_to_string(&ptr.ty) == "c_char" {
                return Ok(Some("String".into()));
            }
            anon_rust_to_java(&ptr.ty)
        }

        // Plain old types.
        ast::TyKind::Path(None, ref path) => path_to_java(path),

        // Possibly void, likely not.
        _ => {
            let new_type = pprust::ty_to_string(ty);
            if new_type == "()" {
                Ok(Some("void".into()))
            } else {
                Err(Error {
                    level: Level::Error,
                    span: Some(ty.span),
                    message: format!("cheddar can not handle the type `{}`", new_type),
                })
            }
        }
    }
}

/// Convert a Rust path type (my_mod::MyType) to a C type.
///
/// Types hidden behind modules are almost certainly custom types (which wouldn't work) except
/// types in `libc` which we special case.
fn path_to_java(path: &ast::Path) -> Result<Option<String>, Error> {
    // I don't think this is possible.
    if path.segments.is_empty() {
        Err(Error {
            level: Level::Bug,
            span: Some(path.span),
            message: "what the fuck have you done to this type?!".into(),
        })
    // Types in modules, `my_mod::MyType`.
    } else if path.segments.len() > 1 {
        let (ty, module) = path.segments.split_last().expect(
            "already checked that there were at least two elements",
        );
        let ty: &str = &ty.identifier.name.as_str();
        let mut segments = Vec::with_capacity(module.len());
        for segment in module {
            segments.push(String::from(&*segment.identifier.name.as_str()));
        }
        let module = segments.join("::");
        match &*module {
            "libc" => Ok(Some(libc_ty_to_java(ty).into())),
            "std::os::raw" => Ok(Some(osraw_ty_to_java(ty).into())),
            _ => Err(Error {
                level: Level::Error,
                span: Some(path.span),
                message: "cheddar can not handle types in other modules (except `libc` and `std::os::raw`)"
                    .into(),
            }),
        }
    } else {
        Ok(Some(
            rust_ty_to_java(&path.segments[0].identifier.name.as_str())
                .into(),
        ))
    }
}

/// Convert a Rust type from `libc` into a C type.
///
/// Most map straight over but some have to be converted.
fn libc_ty_to_java(ty: &str) -> &str {
    match ty {
        "c_void" => "void",
        "c_float" => "float",
        "c_double" => "double",
        "c_char" => "byte",
        "c_schar" => "byte",
        "c_uchar" => "byte",
        "c_short" => "short",
        "c_ushort" => "short",
        "c_int" => "int",
        "c_uint" => "int",
        "c_long" => "long",
        "c_ulong" => "long",
        // All other types should map over to C.
        ty => ty,
    }
}

/// Convert a Rust type from `std::os::raw` into a C type.
///
/// These mostly mirror the libc crate.
fn osraw_ty_to_java(ty: &str) -> &str {
    match ty {
        "c_void" => "void",
        "c_char" => "byte",
        "c_double" => "double",
        "c_float" => "float",
        "c_int" => "int",
        "c_long" => "long",
        "c_schar" => "byte",
        "c_short" => "short",
        "c_uchar" => "byte",
        "c_uint" => "int",
        "c_ulong" => "long",
        "c_ushort" => "short",
        // All other types should map over to C.
        ty => ty,
    }
}

/// Convert any Rust type into C.
///
/// This includes user-defined types. We currently trust the user not to use types which we don't
/// know the structure of (like String).
fn rust_ty_to_java(ty: &str) -> &str {
    match ty {
        "()" => "void",
        "f32" => "float",
        "f64" => "double",
        "i8" => "byte",
        "i16" => "short",
        "i32" => "int",
        "i64" => "long",
        "isize" => "intptr_t",
        "u8" => "byte",
        "u16" => "short",
        "u32" => "int",
        "u64" => "long",
        "usize" => "long",
        // This is why we write out structs and enums as `typedef ...`.
        // We `#include <stdbool.h>` so bool is handled.
        ty => libc_ty_to_java(ty),
    }
}