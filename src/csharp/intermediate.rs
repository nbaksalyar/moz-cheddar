//! Types and utilities for the intermediate representation between the rust code
//! and the target language code.

use common;
use syntax::ast;
use syntax::print::pprust;

pub enum Type {
    Unit,
    Bool,
    CChar,
    F32,
    F64,
    I8,
    I16,
    I32,
    I64,
    ISize,
    U8,
    U16,
    U32,
    U64,
    USize,
    String,
    Pointer(Box<Type>),
    Array(Box<Type>, usize),
    Function(Box<Function>),
    User(String),
}

pub struct Function {
    pub inputs: Vec<(String, Type)>,
    pub output: Type,
}

pub fn transform_type(input: &ast::Ty) -> Option<Type> {
    match input.node {
        ast::TyKind::Array(ref ty, ref size) => transform_array(ty, size),
        ast::TyKind::Path(None, _) => transform_path(input),
        ast::TyKind::Ptr(ref ptr) => transform_pointer(ptr),
        ast::TyKind::BareFn(ref bare_fn) => {
            transform_function(&*bare_fn.decl).map(|fun| Type::Function(Box::new(fun)))
        }
        _ => None,
    }
}

pub fn transform_function(decl: &ast::FnDecl) -> Option<Function> {
    let output = match decl.output {
        ast::FunctionRetTy::Default(..) => Type::Unit,
        ast::FunctionRetTy::Ty(ref ty) => {
            match transform_type(ty) {
                Some(ty) => ty,
                None => return None,
            }
        }
    };

    let inputs: Option<_> = decl.inputs
        .iter()
        .map(|arg| {
            let ty = match transform_type(&*arg.ty) {
                Some(ty) => ty,
                None => return None,
            };

            let name = pprust::pat_to_string(&*arg.pat);

            Some((name, ty))
        })
        .collect();
    let inputs = match inputs {
        Some(inputs) => inputs,
        None => return None,
    };

    Some(Function { inputs, output })
}

fn transform_array(ty: &ast::Ty, size: &ast::Expr) -> Option<Type> {
    let size = match extract_int_literal(size) {
        None => return None,
        Some(size) => size as usize,
    };

    let ty = match transform_type(ty) {
        None => return None,
        Some(Type::Array { .. }) => return None, // multi-dimensional array not supported yet
        Some(ty) => ty,
    };

    Some(Type::Array(Box::new(ty), size))
}

fn transform_path(input: &ast::Ty) -> Option<Type> {
    let full = pprust::ty_to_string(input);
    let output = match full.as_str() {
        "bool" => Type::Bool,
        "c_char" |
        "libc::c_char" |
        "std::os::raw::c_char" => Type::CChar,
        "f32" => Type::F32,
        "f64" => Type::F64,
        "i8" => Type::I8,
        "i16" => Type::I16,
        "i32" => Type::I32,
        "i64" => Type::I64,
        "isize" => Type::ISize,
        "u8" => Type::U8,
        "u16" => Type::U16,
        "u32" => Type::U32,
        "u64" => Type::U64,
        "usize" => Type::USize,
        "c_void" |
        "libc::c_void" |
        "std::os::raw::c_void" => Type::Unit,
        name => Type::User(name.to_string()),
    };

    Some(output)
}

fn transform_pointer(ptr: &ast::MutTy) -> Option<Type> {
    match transform_type(&ptr.ty) {
        Some(Type::CChar) => Some(Type::String),
        Some(Type::User(name)) => Some(Type::User(name)),
        Some(ty) => Some(Type::Pointer(Box::new(ty))),
        _ => None,
    }
}


pub fn is_user_data(name: &str, ty: &Type) -> bool {
    if let Type::Pointer(ref ty) = *ty {
        if let Type::Unit = **ty {
            return name == "user_data";
        }
    }

    false
}

pub fn extract_callback(ty: &Type) -> Option<&Function> {
    if let Type::Function(ref fun) = *ty {
        if let Some(first) = fun.inputs.first() {
            if is_user_data(&first.0, &first.1) {
                return Some(fun);
            }
        }
    }

    None
}

pub fn extract_callbacks(inputs: &[(String, Type)]) -> Vec<(&str, &Function)> {
    inputs
        .iter()
        .filter_map(|&(ref name, ref ty)| {
            extract_callback(ty).map(|fun| (name.as_str(), fun))
        })
        .collect()
}

pub fn callback_arity(fun: &Function) -> usize {
    // Do not count the user_data param.
    fun.inputs.len() - 1
}

pub fn extract_enum_variant_value(variant: &ast::Variant) -> Option<u64> {
    if let Some(ref expr) = variant.node.disr_expr {
        extract_int_literal(expr)
    } else {
        None
    }
}

fn extract_int_literal(expr: &ast::Expr) -> Option<u64> {
    if let ast::ExprKind::Lit(ref lit) = expr.node {
        let ast::Lit { ref node, .. } = *&**lit;
        if let ast::LitKind::Int(val, ..) = *node {
            return Some(val);
        }
    }

    None
}

pub fn retrieve_docstring(attr: &ast::Attribute) -> Option<String> {
    common::retrieve_docstring(attr, "")
}
