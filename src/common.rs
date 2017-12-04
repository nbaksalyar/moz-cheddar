//! Functions common for all target languages.

use Error;
use std::collections::HashMap;
use std::path::PathBuf;
use syntax::abi::Abi;
use syntax::ast;
use syntax::print::pprust;

/// Outputs several files as a result of an AST transformation.
pub type Outputs = HashMap<PathBuf, String>;

/// Target language support
pub trait Lang {
    /// Convert a Rust constant (`pub const NAME: Type = value;`) into a target
    /// language constant.
    fn parse_const(&mut self, _item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        Ok(())
    }

    /// Convert `pub type A = B;` into `typedef B A;`.
    fn parse_ty(&mut self, _item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        Ok(())
    }

    /// Convert a Rust enum into a target language enum.
    fn parse_enum(&mut self, _item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        Ok(())
    }

    /// Convert a Rust struct into a target language struct.
    fn parse_struct(&mut self, _item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        Ok(())
    }

    /// Convert a Rust function declaration into a target language function declaration.
    fn parse_fn(&mut self, _item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        Ok(())
    }

    /// Add extra and custom code after the code generation part is done.
    fn finalise_output(&mut self, _outputs: &mut Outputs) -> Result<(), Error> {
        Ok(())
    }
}

/// Check the attribute is #[no_mangle].
pub fn check_no_mangle(attr: &ast::Attribute) -> bool {
    match attr.value.node {
        ast::MetaItemKind::Word if attr.name() == "no_mangle" => true,
        _ => false,
    }
}

/// Check the function argument is `user_data: *mut c_void`
pub fn is_user_data_arg(arg: &ast::Arg) -> bool {
    pprust::pat_to_string(&*arg.pat) == "user_data" &&
        pprust::ty_to_string(&*arg.ty) == "*mut c_void"
}

/// Check the function argument is `result: *const FfiResult`
pub fn is_result_arg(arg: &ast::Arg) -> bool {
    pprust::pat_to_string(&*arg.pat) == "result" &&
        pprust::ty_to_string(&*arg.ty) == "*const FfiResult"
}

/// Check the function argument is a length argument for a *const u8 pointer
pub fn is_ptr_len_arg(arg: &ast::Arg) -> bool {
    let arg_name = pprust::pat_to_string(&*arg.pat);
    pprust::ty_to_string(&*arg.ty) == "usize" &&
        (arg_name.ends_with("_len") || arg_name == "len" || arg_name == "size")
}


/// Detect array ptrs and skip the length args - e.g. for a case of
/// `ptr: *const u8, ptr_len: usize` we're going to skip the `len` part.
pub fn is_array_arg(arg: &ast::Arg, next_arg: Option<&ast::Arg>) -> bool {
    if let ast::TyKind::Ptr(..) = arg.ty.node {
        !is_result_arg(arg) && next_arg.map(is_ptr_len_arg).unwrap_or(false)
    } else {
        false
    }
}

// TODO: Maybe it would be wise to use syntax::attr here.
/// Loop through a list of attributes.
///
/// Check that at least one attribute matches some criteria (usually #[repr(C)] or #[no_mangle])
/// and optionally retrieve a String from it (usually a docstring).
pub fn parse_attr<C, R>(attrs: &[ast::Attribute], check: C, retrieve: R) -> (bool, String)
where
    C: Fn(&ast::Attribute) -> bool,
    R: Fn(&ast::Attribute) -> Option<String>,
{
    let mut check_passed = false;
    let mut retrieved_str = String::new();
    for attr in attrs {
        // Don't want to accidently set it to false after it's been set to true.
        if !check_passed {
            check_passed = check(attr);
        }
        // If this attribute has any strings to retrieve, retrieve them.
        if let Some(string) = retrieve(attr) {
            retrieved_str.push_str(&string);
        }
    }

    (check_passed, retrieved_str)
}

/// Check the attribute is #[repr(C)].
pub fn check_repr_c(attr: &ast::Attribute) -> bool {
    match attr.value.node {
        ast::MetaItemKind::List(ref word) if attr.name() == "repr" => {
            match word.first() {
                Some(word) => {
                    match word.node {
                        // Return true only if attribute is #[repr(C)].
                        ast::NestedMetaItemKind::MetaItem(ref item) if item.name == "C" => true,
                        _ => false,
                    }
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// If the attribute is  a docstring, indent it the required amount and return it.
pub fn retrieve_docstring(attr: &ast::Attribute, prepend: &str) -> Option<String> {
    match attr.value.node {
        ast::MetaItemKind::NameValue(ref val) if attr.name() == "doc" => {
            match val.node {
                // Docstring attributes omit the trailing newline.
                ast::LitKind::Str(ref docs, _) => Some(format!("{}{}\n", prepend, docs)),
                _ => unreachable!("docs must be literal strings"),
            }
        }
        _ => None,
    }
}

/// Returns whether the calling convention of the function is compatible with
/// C (i.e. `extern "C"`).
pub fn is_extern(abi: Abi) -> bool {
    match abi {
        Abi::C | Abi::Cdecl | Abi::Stdcall | Abi::Fastcall | Abi::System => true,
        _ => false,
    }
}
