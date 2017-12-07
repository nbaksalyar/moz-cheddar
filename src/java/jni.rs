//! Functions to generate JNI bindings

use common::{is_array_arg, is_user_data_arg};
use inflector::Inflector;
use java::{Context, Outputs, callback_name};
use jni::signature::{self, JavaType, TypeSignature};
use quote;
use std::collections::BTreeSet;
use std::collections::hash_map::Entry;
use syntax::ast;
use syntax::print::pprust;
use syntax::symbol;

fn to_jni_arg(arg: &ast::Arg, ty_name: &str) -> quote::Tokens {
    let pat = quote::Ident::new(pprust::pat_to_string(&*arg.pat));
    let ty_name = quote::Ident::new(ty_name);
    quote! { #pat: #ty_name }
}

fn transform_jni_arg(arg: &ast::Arg) -> quote::Tokens {
    match arg.ty.node {
        // Callback
        ast::TyKind::BareFn(ref _bare_fn) => to_jni_arg(arg, "JObject"),

        // Plain old types.
        ast::TyKind::Path(None, ref path) => {
            let (ty, _module) = path.segments.split_last().expect(
                "already checked that there were at least two elements",
            );
            let ty: &str = &ty.identifier.name.as_str();

            let jni_type = match ty {
                "c_char" | "u8" | "i8" => "jbyte",
                "c_short" | "u16" | "i16" => "jshort",
                "c_int" | "u32" | "i32" => "jint",
                "c_long" | "u64" | "i64" => "jlong",
                "c_usize" | "usize" | "isize" => "jlong",
                _ => ty,
            };

            to_jni_arg(arg, jni_type)
        }

        // Standard pointers.
        ast::TyKind::Ptr(ref ptr) => {
            // Detect strings, which are *const c_char or *mut c_char
            match pprust::ty_to_string(&ptr.ty).as_str() {
                "c_char" => to_jni_arg(arg, "JString"),
                "App" | "Authenticator" => to_jni_arg(arg, "jlong"), // Opaque ptr,
                _ => to_jni_arg(arg, "JObject"),
            }
        }

        _ => to_jni_arg(arg, &pprust::ty_to_string(&arg.ty)),

    }
}

fn java_ty_to_signature(s: &str) -> Option<JavaType> {
    match s {
        "long" => Some(JavaType::Primitive(signature::Primitive::Long)),
        "byte[]" => Some(JavaType::Array(
            Box::new(JavaType::Primitive(signature::Primitive::Byte)),
        )),
        _ => None,
    }
}

// Produces a fully qualified class name (i.e. with a Java package)
fn fully_qualified(ty: &str, context: &Context) -> String {
    format!("{}/{}", context.namespace.replace(".", "/"), ty)
}

fn rust_ty_to_signature(ty: &ast::Ty, context: &Context) -> Option<JavaType> {
    match ty.node {
        // Callback
        ast::TyKind::BareFn(ref _bare_fn) => Some(JavaType::Object(From::from("java/lang/Object"))),

        // Plain old types.
        ast::TyKind::Path(None, ref path) => {
            let (ty, _module) = path.segments.split_last().expect(
                "already checked that there were at least two elements",
            );
            let ty: &str = &ty.identifier.name.as_str();

            match ty {
                "c_byte" | "u8" | "i8" => Some(JavaType::Primitive(signature::Primitive::Byte)),
                "c_short" | "u16" | "i16" => Some(JavaType::Primitive(signature::Primitive::Short)),
                "c_int" | "u32" | "i32" => Some(JavaType::Primitive(signature::Primitive::Int)),
                "c_long" | "u64" | "i64" => Some(JavaType::Primitive(signature::Primitive::Long)),
                "c_usize" | "usize" | "isize" => Some(
                    JavaType::Primitive(signature::Primitive::Long),
                ),
                "c_bool" | "bool" => Some(JavaType::Object(From::from("java/lang/Boolean"))),
                _ => {
                    if let Some(mapped) = context.type_map.get(ty) {
                        java_ty_to_signature(mapped).or_else(|| {
                            Some(JavaType::Object(From::from(fully_qualified(ty, context))))
                        })
                    } else {
                        Some(JavaType::Object(From::from(fully_qualified(ty, context))))
                    }
                }
            }
        }

        // Standard pointers.
        ast::TyKind::Ptr(ref ptr) => {
            // Detect strings, which are *const c_char or *mut c_char
            if pprust::ty_to_string(&ptr.ty) == "c_char" {
                Some(JavaType::Object(From::from("java/lang/String")))
            } else {
                rust_ty_to_signature(&ptr.ty, context)
            }
        }

        _ => None,

    }
}

struct JniArgResult {
    stmt: quote::Tokens,
    call_args: Vec<quote::Tokens>,
}

fn transform_string_arg(arg_name: &str) -> JniArgResult {
    // statements
    let arg_name = quote::Ident::new(arg_name);
    let stmt =
        quote! {
            let #arg_name = CString::from_java(&env, #arg_name);
        };

    // call arg value(s)
    let call_args = vec![quote! { #arg_name.as_ptr() }];

    JniArgResult { stmt, call_args }
}

fn transform_struct_arg(arg_name: &str, arg_ty: &ast::Ty) -> JniArgResult {
    // statements
    let arg_name = quote::Ident::new(arg_name);
    let struct_ty = quote::Ident::new(pprust::ty_to_string(arg_ty));
    let stmt =
        quote! {
            let #arg_name = #struct_ty::from_java(&env, #arg_name);
        };

    // call arg value(s)
    let call_args = vec![quote! { &#arg_name }];

    JniArgResult { stmt, call_args }
}

fn transform_array_arg(arg_name: &str) -> JniArgResult {
    // statements
    let arg_name = quote::Ident::new(arg_name);
    let stmt =
        quote! {
            let #arg_name = Vec::from_java(&env, #arg_name);
        };

    // call arg value(s)
    let call_args = vec![quote! { #arg_name.as_ptr() }, quote! { #arg_name.len() }];

    JniArgResult { stmt, call_args }
}

fn transform_callbacks_arg(
    cb_idents: &[(ast::BareFnTy, quote::Ident)],
    cb_base_name: &str,
) -> JniArgResult {
    // statements
    let cb_ids: Vec<quote::Ident> = cb_idents
        .iter()
        .map(|&(_, ref ident)| ident.clone())
        .collect();

    let stmt =
        quote! {
            let ctx = gen_ctx!(env, #(#cb_ids),*);
        };

    // call arg value(s)
    let multi_callback = cb_idents.len() > 1;

    let call_args = cb_idents
        .iter()
        .enumerate()
        .map(|(idx, _)| {
            let cb_fn = if multi_callback {
                quote::Ident::new(format!("{}_{}", cb_base_name, idx))
            } else {
                quote::Ident::new(cb_base_name)
            };
            quote! { #cb_fn }
        })
        .collect();

    JniArgResult { stmt, call_args }
}

fn transform_opaque_ptr(arg_name: &str, ty: &str) -> JniArgResult {
    // statements
    let arg_name = quote::Ident::new(arg_name);
    let ty = quote::Ident::new(ty);
    let stmt =
        quote! {
            let #arg_name = #arg_name as *mut #ty;
        };

    // call arg value(s)
    let call_args = vec![quote! { #arg_name }];

    JniArgResult { stmt, call_args }
}

/// Generates JNI function binding based on a native function
pub fn generate_jni_function(
    args: Vec<ast::Arg>,
    native_name: &str,
    func_name: &str,
    context: &mut Context,
    outputs: &mut Outputs,
) -> String {
    let func_name = quote::Ident::new(format!(
        "Java_{}_NativeBindings_{}",
        context.namespace.replace("_", "_1").replace(".", "_"),
        func_name
    ));
    let native_name_str = native_name;
    let native_name = quote::Ident::new(native_name);

    // Generate inputs
    let mut call_args = Vec::new();
    let mut stmts = Vec::new();
    let mut callbacks = Vec::new();
    let mut jni_fn_inputs = Vec::new();

    let mut args_iter = args.into_iter()
        .filter(|arg| !is_user_data_arg(arg))
        .peekable();

    while let Some(arg) = args_iter.next() {
        let arg_name = pprust::pat_to_string(&*arg.pat);

        let res = if is_array_arg(&arg, args_iter.peek()) {
            args_iter.next();
            Some(transform_array_arg(&arg_name))
        } else {
            match arg.ty.node {
                // Callback
                ast::TyKind::BareFn(ref bare_fn) => {
                    callbacks.push((bare_fn.clone().unwrap(), quote::Ident::new(arg_name)));
                    None
                }

                // Pointers
                ast::TyKind::Ptr(ref ptr) => {
                    match pprust::ty_to_string(&ptr.ty).as_str() {
                        // Opaque pointer that should be passed as a long value
                        opaque @ "App" |
                        opaque @ "Authenticator" => Some(transform_opaque_ptr(&arg_name, opaque)),
                        // Detect strings, which are *const c_char or *mut c_char
                        "c_char" => Some(transform_string_arg(&arg_name)),
                        _ => Some(transform_struct_arg(&arg_name, &ptr.ty)),
                    }
                }

                // Native types and others
                _ => {
                    let id = quote::Ident::new(arg_name);
                    let native_ty = quote::Ident::new(pprust::ty_to_string(&arg.ty));

                    Some(JniArgResult {
                        stmt: quote!{},
                        call_args: vec![quote! { #id as #native_ty }],
                    })
                }
            }
        };

        if let Some(jni_arg_res) = res {
            call_args.extend(jni_arg_res.call_args);
            stmts.push(jni_arg_res.stmt);
        }

        jni_fn_inputs.push(transform_jni_arg(&arg));
    }

    if callbacks.len() > 0 {
        let cb_base_name = if callbacks.len() > 1 {
            format!("call_{}", native_name_str)
        } else {
            let &(ref cb, _) = &callbacks[0];
            format!("call_{}", callback_name(&*cb.decl.inputs, context).unwrap())
        };

        let cb_arg_res = transform_callbacks_arg(&callbacks, &cb_base_name);
        call_args.push(quote! { ctx });
        call_args.extend(cb_arg_res.call_args);
        stmts.push(cb_arg_res.stmt);
    }

    if callbacks.len() > 1 {
        // Generate extra callbacks for multi-callback functions
        let count = callbacks.len();

        for (idx, &(ref cb, _)) in callbacks.iter().enumerate() {
            let full_cb_name = format!("call_{}_{}", native_name_str, idx);
            eprintln!("Generating JNI CB {}", full_cb_name);

            if !context.generated_jni_cbs.contains(&full_cb_name) {
                let jni = generate_multi_jni_callback(cb, &full_cb_name, idx, count, context);

                match outputs.entry(From::from("jni.rs")) {
                    Entry::Occupied(o) => o.into_mut().push_str(&jni),
                    Entry::Vacant(v) => {
                        let _ = v.insert(jni);
                    }
                }

                context.generated_jni_cbs.insert(full_cb_name);
            }
        }
    }

    let native_lib = quote::Ident::new(context.lib_name.clone());

    let tokens =
        quote! {
            #[no_mangle]
            pub unsafe extern "system" fn #func_name(env: JNIEnv, _class: JClass, #(#jni_fn_inputs),*) {
                #(#stmts)*
                #native_lib::#native_name(#(#call_args),*);
            }
        };

    tokens.to_string()
}

/// Transform `ast::Arg` into an (identifier, type) tuple
fn transform_arg(arg: &ast::Arg) -> (quote::Ident, quote::Ident) {
    (
        quote::Ident::new(pprust::pat_to_string(&*arg.pat)),
        quote::Ident::new(pprust::ty_to_string(&*arg.ty)),
    )
}

struct JniCallback {
    // Native function call parameters
    args: Vec<quote::Tokens>,
    // Callback function statements
    stmts: Vec<quote::Tokens>,
    // Arguments for the callback function
    jni_cb_inputs: Vec<quote::Tokens>,
    // String Java type signature constructor
    arg_ty_str: String,
}

fn generate_callback(cb: &ast::BareFnTy, context: &Context) -> JniCallback {
    let mut args: Vec<quote::Tokens> = Vec::new();
    let mut stmts: Vec<quote::Tokens> = Vec::new();
    let mut jni_cb_inputs = Vec::new();
    let mut arg_java_ty = Vec::new();

    let mut args_iter = (&*cb.decl)
        .inputs
        .iter()
        .filter(|arg| !is_user_data_arg(arg))
        .peekable();

    while let Some(arg) = args_iter.next() {
        let (arg_name, arg_ty) = transform_arg(&arg);

        jni_cb_inputs.push(quote! { #arg_name: #arg_ty });
        args.push(quote! { #arg_name.into() });

        if is_array_arg(&arg, args_iter.peek().cloned()) {
            // Handle array arguments
            let val_java_type = rust_ty_to_signature(&arg.ty, context).unwrap();
            arg_java_ty.push(JavaType::Array(Box::new(val_java_type)));

            if let Some(len_arg) = args_iter.next() {
                let (len_arg_name, len_arg_ty) = transform_arg(&len_arg);
                jni_cb_inputs.push(quote! { #len_arg_name: #len_arg_ty });

                stmts.push(quote! {
                    let #arg_name = slice::from_raw_parts(#arg_name, #len_arg_name).to_java(&env);
                });
            } else {
                // error: no length arg?
            }
        } else {
            let stmt = match arg.ty.node {
                // Pointers
                ast::TyKind::Ptr(ref ptr) => {
                    match pprust::ty_to_string(&ptr.ty).as_str() {
                        // Opaque ptrs passed as long values
                        "App" | "Authenticator" => {
                            quote! {
                                let #arg_name = #arg_name as jlong;
                            }
                        }
                        // Strings
                        "c_char" => {
                            quote! {
                                let #arg_name: JObject = #arg_name.to_java(&env).into();
                            }
                        }
                        // Other ptrs
                        _ => {
                            quote! {
                            let #arg_name = (*#arg_name).to_java(&env);
                        }
                        }
                    }
                }
                _ => {
                    quote! {
                        let #arg_name = #arg_name.to_java(&env);
                    }
                }
            };

            arg_java_ty.push(rust_ty_to_signature(&arg.ty, context).unwrap());
            stmts.push(stmt);
        }
    }

    let arg_ty_str = format!(
        "{}",
        TypeSignature {
            args: arg_java_ty,
            ret: JavaType::Primitive(signature::Primitive::Void),
        }
    );

    JniCallback {
        args,
        stmts,
        jni_cb_inputs,
        arg_ty_str,
    }
}

fn generate_multi_jni_callback(
    cb: &ast::BareFnTy,
    cb_name: &str,
    callback_index: usize,
    callbacks_count: usize,
    context: &mut Context,
) -> String {
    let cb_name = quote::Ident::new(cb_name);

    let JniCallback {
        args,
        jni_cb_inputs,
        stmts,
        arg_ty_str,
    } = generate_callback(cb, context);

    let tokens =
        quote! {
        extern "C" fn #cb_name(ctx: *mut c_void, #(#jni_cb_inputs),*) {
            unsafe {
                let env = JVM.as_ref()
                    .map(|vm| vm.attach_current_thread_as_daemon().unwrap())
                    .unwrap();

                let mut cbs = Box::from_raw(ctx as *mut [Option<GlobalRef>; #callbacks_count]);

                if let Some(cb) = cbs[#callback_index].take() {
                    #(#stmts);*

                    env.call_method(
                        cb.as_obj(),
                        "call",
                        #arg_ty_str,
                        &[ #(#args),* ],
                    ).unwrap();
                }

                if cbs.iter().any(|cb| cb.is_some()) {
                    mem::forget(cbs);
                }
            }
        }
    };

    tokens.to_string()
}

/// Generates a JNI callback function based on a native callback type
pub fn generate_jni_callback(cb: &ast::BareFnTy, cb_name: &str, context: &mut Context) -> String {
    let cb_name = quote::Ident::new(cb_name);

    let JniCallback {
        args,
        jni_cb_inputs,
        stmts,
        arg_ty_str,
    } = generate_callback(cb, context);

    let tokens =
        quote! {
        extern "C" fn #cb_name(ctx: *mut c_void, #(#jni_cb_inputs),*) {
            unsafe {
                let env = JVM.as_ref()
                    .map(|vm| vm.attach_current_thread_as_daemon().unwrap())
                    .unwrap();
                let cb = convert_cb_from_java(&env, ctx);

                #(#stmts);*

                env.call_method(
                    cb.as_obj(),
                    "call",
                    #arg_ty_str,
                    &[ #(#args),* ],
                ).unwrap();
            }
        }
    };

    tokens.to_string()
}

enum StructField {
    Primitive(ast::StructField),
    Array {
        field: ast::StructField,
        len_field: String,
        cap_field: Option<String>,
    },
    String(ast::StructField),
    StructPtr {
        field: ast::StructField,
        ty: ast::MutTy,
    },
    LenField(ast::StructField),
}

impl StructField {
    fn struct_field(&self) -> &ast::StructField {
        match *self {
            StructField::Primitive(ref f) => f,
            StructField::Array { field: ref f, .. } => f,
            StructField::StructPtr { field: ref f, .. } => f,
            StructField::String(ref f) => f,
            StructField::LenField(ref f) => f,
        }
    }

    fn name(&self) -> symbol::InternedString {
        self.struct_field().ident.unwrap().name.as_str()
    }
}

fn transform_struct_fields(fields: &[ast::StructField]) -> Vec<StructField> {
    let mut results = Vec::new();
    let field_names: BTreeSet<_> = fields
        .iter()
        .map(|f| f.ident.unwrap().name.as_str().to_string())
        .collect();

    for f in fields {
        let mut field_name: String = f.ident.unwrap().name.as_str().to_string();

        match f.ty.node {
            // Pointers
            ast::TyKind::Ptr(ref ptr) => {
                if field_name.ends_with("_ptr") {
                    field_name = field_name.chars().take(field_name.len() - 4).collect();
                }

                let len_field = format!("{}_len", field_name);
                let cap_field = format!("{}_cap", field_name);

                if field_names.contains(&len_field) {
                    results.push(StructField::Array {
                        field: f.clone(),
                        len_field,
                        cap_field: if field_names.contains(&cap_field) {
                            Some(cap_field)
                        } else {
                            None
                        },
                    });
                } else {
                    match pprust::ty_to_string(&ptr.ty).as_str() {
                        // Strings
                        "c_char" => {
                            results.push(StructField::String(f.clone()));
                        }
                        // Other ptrs, most likely structs
                        _ => {
                            results.push(StructField::StructPtr {
                                field: f.clone(),
                                ty: ptr.clone(),
                            });
                        }
                    }
                }
            }

            ast::TyKind::Path(None, ref _path) => {
                results.push(if is_array_meta_field(f) {
                    StructField::LenField(f.clone())
                } else {
                    StructField::Primitive(f.clone())
                });
            }

            _ => results.push(StructField::Primitive(f.clone())),
        }
    }

    results
}

fn is_array_meta_field(field: &ast::StructField) -> bool {
    let str_name = field.ident.unwrap().name.as_str();

    if let ast::TyKind::Path(None, ref path) = field.ty.node {
        let (ty, _module) = path.segments.split_last().expect(
            "already checked that there were at least two elements",
        );
        let ty: &str = &ty.identifier.name.as_str();

        ty == "usize" && (str_name.ends_with("_len") || str_name.ends_with("_cap"))
    } else {
        false
    }
}

fn generate_struct_to_java(
    struct_ident: &quote::Ident,
    java_class_name: &str,
    fields: &[ast::StructField],
    context: &Context,
) -> quote::Tokens {
    let fields = transform_struct_fields(fields);
    let mut stmts = Vec::new();

    for f in fields {
        let field_name_str: &str = &f.name();
        let field_name = quote::Ident::new(field_name_str);
        let java_field_name = field_name_str.to_camel_case();

        let stmt = match f {
            StructField::Array { len_field, field, .. } => {
                if let ast::TyKind::Ptr(ref ptr) = field.ty.node {
                    let len_field_ident = quote::Ident::new(len_field.clone());
                    let len_field = len_field.to_camel_case();
                    let ty_str = pprust::ty_to_string(&ptr.ty);

                    if ty_str.as_str() == "u8" || ty_str.as_str() == "i8" {
                        // Byte array
                        quote! {
                            let arr = env.new_byte_array(self.#len_field_ident as jni::sys::jsize).unwrap();
                            let slice = unsafe { slice::from_raw_parts(self.#field_name as *const i8, self.#len_field_ident) };
                            env.set_byte_array_region(arr, 0, slice).unwrap();
                            env.set_field(output, #java_field_name, "[B", JObject::from(arr).into()).unwrap();
                            env.set_field(output, #len_field, "J", self.#len_field_ident.to_java(&env).into()).unwrap();
                        }
                    } else {
                        // Struct array
                        quote! {
                            let arr = env.new_object_array(self.#len_field_ident as jni::sys::jsize, #ty_str, JObject::null()).unwrap();
                            let items = unsafe { slice::from_raw_parts(self.#field_name, self.#len_field_ident) };
                            for (idx, item) in items.iter().enumerate() {
                                env.set_object_array_element(arr, idx as jni::sys::jsize, item.to_java(env)).unwrap();
                            }
                            env.set_field(output, #java_field_name, "[Ljava/lang/Object;", JObject::from(arr).into()).unwrap();
                            env.set_field(output, #len_field, "J", self.#len_field_ident.to_java(&env).into()).unwrap();
                        }
                    }
                } else {
                    quote!{}
                }
            }
            StructField::String(ref _f) => {
                quote! {
                    if !self.#field_name.is_null() {
                        let #field_name: JObject = self.#field_name.to_java(&env).into();
                        env.set_field(output, #java_field_name, "Ljava/lang/String;", #field_name.into())
                            .unwrap();
                    }
                }
            }
            StructField::StructPtr { .. } => {
                quote! {
                    env.set_field(output, #field_name_str, "Ljava/lang/Object;", self.#field_name.to_java(&env).into()).unwrap();
                }
            }
            StructField::LenField(ref _f) => {
                // Skip len/cap fields transformation - it's covered by `ArrayField`
                quote!{}
            }
            StructField::Primitive(ref f) => {
                match f.ty.node {
                    ast::TyKind::Path(None, ref path) => {
                        let (ty, _module) = path.segments.split_last().expect(
                            "already checked that there were at least two elements",
                        );
                        let ty: &str = &ty.identifier.name.as_str();

                        let conv = match ty {
                            "c_byte" | "i8" | "u8" => Some("B"),
                            "c_short" | "u16" | "i16" => Some("S"),
                            "c_int" | "u32" | "i32" => Some("I"),
                            "c_long" | "u64" | "i64" | "c_usize" | "usize" | "isize" => Some("J"),
                            "c_bool" | "bool" => Some("Z"),
                            _ => None,
                        };

                        if let Some(signature) = conv {
                            quote! {
                                env.set_field(output, #java_field_name, #signature, self.#field_name.to_java(&env).into()).unwrap();
                            }
                        } else {
                            quote!{
                                env.set_field(output, #java_field_name, "Ljava/lang/Object;", self.#field_name.to_java(&env).into()).unwrap();
                            }
                        }
                    }
                    _ => quote!{},
                }
            }
        };

        stmts.push(stmt);
    }

    let fully_qualified_name = fully_qualified(java_class_name, context);

    quote! {
        impl<'a> ToJava<'a, JObject<'a>> for #struct_ident {
            fn to_java(&self, env: &'a JNIEnv) -> JObject<'a> {
                let output = env.new_object(#fully_qualified_name, "()V", &[]).unwrap();
                #(#stmts)*
                output
            }
        }
    }
}

fn generate_struct_from_java(
    struct_ident: &quote::Ident,
    fields: &[ast::StructField],
    context: &Context,
) -> quote::Tokens {
    let fields = transform_struct_fields(fields);
    let mut fields_values = Vec::new();
    let mut conversions = Vec::new();

    for f in fields {
        let field_name_str: &str = &f.name();
        let field_name = quote::Ident::new(field_name_str);
        let java_field_name = field_name_str.to_camel_case();

        fields_values.push(quote! {
            #field_name
        });

        let conv = match f {
            StructField::Array {
                len_field,
                cap_field,
                field,
            } => {
                let len_field = quote::Ident::new(len_field);

                let cap = if let Some(cap_field) = cap_field {
                    // If there's a capacity field in the struct, just get it from the
                    // generated Vec itself.
                    let cap_field = quote::Ident::new(cap_field);
                    quote! {
                        let #cap_field = vec.capacity();
                    }
                } else {
                    quote!{}
                };

                if let ast::TyKind::Ptr(ref ptr) = field.ty.node {
                    let ty_str = pprust::ty_to_string(&ptr.ty);

                    let ptr_mutability = if let ast::Mutability::Mutable = ptr.mutbl {
                        quote! { as_mut_ptr }
                    } else {
                        quote! { as_ptr }
                    };

                    if ty_str.as_str() == "u8" {
                        // Byte array
                        quote! {
                            let arr = env.get_field(input, #field_name_str, "[Ljava/lang/Object;").unwrap().l().unwrap().into_inner() as jni::sys::jbyteArray;
                            let mut vec = env.convert_byte_array(arr).unwrap();
                            let #len_field = vec.len();
                            #cap
                            let #field_name = vec.#ptr_mutability();
                            ::std::mem::forget(vec);
                        }
                    } else {
                        // Struct array
                        let ty = quote::Ident::new(ty_str);

                        quote! {
                            let arr = env.get_field(input, #field_name_str, "[Ljava/lang/Object;").unwrap().l().unwrap().into_inner() as jni::sys::jarray;
                            let #len_field = env.get_array_length(arr).unwrap() as usize;

                            let mut vec = Vec::with_capacity(#len_field);

                            for idx in 0..#len_field {
                                let item = env.get_object_array_element(arr, idx as jni::sys::jsize);
                                let item = #ty::from_java(&env, item.unwrap());
                                vec.push(item);
                            }

                            #cap
                            let #field_name = vec.#ptr_mutability();
                            ::std::mem::forget(vec);
                        }
                    }
                } else {
                    quote!{}
                }
            }
            StructField::StructPtr { ty, .. } => {
                let ty_str = pprust::ty_to_string(&ty.ty);
                let ty = quote::Ident::new(ty_str);

                quote! {
                    let #field_name = env.get_field(input, #field_name_str, "Ljava/lang/Object;").unwrap().l().unwrap();
                    let #field_name = #ty::from_java(&env, #field_name);
                }
            }
            StructField::LenField(ref _f) => {
                // Skip len/cap fields transformation - it's covered by `ArrayField`
                quote!{}
            }
            StructField::String(ref _f) => {
                quote! {
                    let #field_name = env.get_field(input, #field_name_str, "Ljava/lang/String;")
                        .unwrap()
                        .l()
                        .unwrap()
                        .into();
                    let #field_name = <*mut _>::from_java(env, #field_name);
                }
            }
            StructField::Primitive(ref f) => {
                match f.ty.node {
                    ast::TyKind::Path(None, ref path) => {
                        let (ty, _module) = path.segments.split_last().expect(
                            "already checked that there were at least two elements",
                        );
                        let mut ty: &str = &ty.identifier.name.as_str();

                        if let Some(rewrite_ty) = context.type_map.get(ty) {
                            // Rewrite type (it could be e.g. a handle)
                            ty = match *rewrite_ty {
                                "long" => "u64",
                                _ => ty,
                            };
                        }

                        let rust_ty = quote::Ident::new(ty);

                        let conv = match ty {
                            "c_byte" | "i8" | "u8" => Some(("B", quote! { b() })),
                            "c_short" | "u16" | "i16" => Some(("S", quote! { s() })),
                            "c_int" | "u32" | "i32" => Some(("I", quote! { i() })),
                            "c_long" | "u64" | "i64" | "c_usize" | "usize" | "isize" => Some((
                                "J",
                                quote! { j() },
                            )),
                            "c_bool" | "bool" => Some(("Z", quote! { z() })),
                            _ => None,
                        };

                        if let Some(conv) = conv {
                            let signature = conv.0;
                            let unwrap_method = conv.1;

                            quote! {
                                let #field_name = env.get_field(input, #java_field_name, #signature).unwrap().#unwrap_method.unwrap() as #rust_ty;
                            }
                        } else {
                            quote!{
                                let #field_name = env.get_field(input, #java_field_name, "Ljava/lang/Object;").unwrap().l().unwrap();
                                let #field_name = #rust_ty::from_java(&env, #field_name);
                            }
                        }
                    }
                    _ => quote!{},
                }
            }
        };

        conversions.push(conv);
    }

    quote! {
        impl<'a> FromJava<JObject<'a>> for #struct_ident {
            fn from_java(env: &JNIEnv, input: JObject) -> Self {
                #(#conversions)*

                #struct_ident {
                    #(#fields_values),*
                }
            }
        }
    }

}

/// Generates JNI struct binding based on a native struct
pub fn generate_struct(
    fields: &[ast::StructField],
    native_name: &str,
    java_class_name: &str,
    context: &Context,
) -> String {
    let struct_ident = quote::Ident::new(native_name);

    let from_java = generate_struct_from_java(&struct_ident, fields, context);
    let to_java = generate_struct_to_java(&struct_ident, java_class_name, fields, context);

    let tokens =
        quote! {
            #from_java

            #to_java
        };

    tokens.to_string()
}
