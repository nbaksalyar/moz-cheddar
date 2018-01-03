#[macro_use]
mod emit;
mod intermediate;
#[cfg(test)]
mod tests;

use self::emit::*;
use self::intermediate::*;
use Error;
use Level;
use common::{self, FilterMode, Lang, Outputs};
use inflector::Inflector;
use output::IndentedWriter;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::collections::btree_map::Entry;
use std::fmt::{Display, Write};
use std::mem;
use std::path::PathBuf;
use syntax::ast;
use syntax::print::pprust;

const INDENT_WIDTH: usize = 2;

pub struct LangCSharp {
    filter: HashSet<String>,
    filter_mode: FilterMode,
    wrapper_function_blacklist: HashSet<String>,
    types_enabled: bool,
    utils_enabled: bool,
    context: Context,
    custom_consts: Vec<String>,
    consts: Vec<Snippet<Const>>,
    enums: Vec<Snippet<Enum>>,
    structs: Vec<Snippet<Struct>>,
    functions: Vec<Snippet<Function>>,
    aliases: HashMap<String, Type>,
}

pub struct Context {
    lib_name: String,
    namespace: String,
    class_name: String,
    consts_class_name: String,
    types_file_name: String,
    utils_class_name: String,
    preserve_comments: bool,
    opaque_types: HashSet<String>,
    native_types: HashSet<String>,
}

impl Context {
    pub fn is_opaque(&self, name: &str) -> bool {
        self.opaque_types.contains(name)
    }

    pub fn is_native_name(&self, name: &str) -> bool {
        self.native_types.contains(name)
    }

    pub fn is_native_type(&self, ty: &Type) -> bool {
        match *ty {
            Type::Pointer(ref ty) => self.is_native_type(&*ty),
            Type::User(ref name) => self.is_native_name(name),
            _ => false,
        }
    }
}

impl LangCSharp {
    pub fn new() -> Self {
        LangCSharp {
            filter_mode: FilterMode::Blacklist,
            filter: Default::default(),
            wrapper_function_blacklist: Default::default(),
            types_enabled: true,
            utils_enabled: true,
            context: Context {
                lib_name: "backend".to_string(),
                namespace: "Backend".to_string(),
                class_name: "Backend".to_string(),
                consts_class_name: "Constants".to_string(),
                types_file_name: "Types".to_string(),
                utils_class_name: "Utils".to_string(),
                preserve_comments: false,
                opaque_types: Default::default(),
                native_types: Default::default(),
            },
            custom_consts: Vec::new(),
            consts: Vec::new(),
            enums: Vec::new(),
            structs: Vec::new(),
            functions: Vec::new(),
            aliases: Default::default(),
        }
    }

    /// Set the name of the native library. This also sets the class name.
    pub fn set_lib_name<T: Into<String>>(&mut self, name: T) {
        self.context.lib_name = name.into();
    }

    /// Set the namespace to put all the generated code in.
    pub fn set_namespace<T: Into<String>>(&mut self, namespace: T) {
        self.context.namespace = namespace.into();
    }

    /// Set the name of the static class containing all transformed functions and
    /// constants. By default this is derived from the linked library name.
    pub fn set_class_name<T: Into<String>>(&mut self, name: T) {
        self.context.class_name = name.into();
    }

    /// Add definition of opaque type (type represented by an opaque pointer).
    pub fn add_opaque_type<T: Into<String>>(&mut self, name: T) {
        let _ = self.context.opaque_types.insert(name.into());
    }

    /// Set the name of the class containing all constants.
    pub fn set_consts_class_name<T: Into<String>>(&mut self, name: T) {
        self.context.consts_class_name = name.into();
    }

    /// Enabl/disable generation of types.
    pub fn set_types_enabled(&mut self, enabled: bool) {
        self.types_enabled = enabled;
    }

    /// Set the name of the file containing types (structs, enums, ...).
    pub fn set_types_file_name<T: Into<String>>(&mut self, name: T) {
        let mut name = name.into();
        if name.ends_with(".cs") {
            let len = name.len();
            name.truncate(len - 3);
        }

        self.context.types_file_name = name;
    }

    /// Set the name of the utils class.
    pub fn set_utils_class_name<T: Into<String>>(&mut self, name: T) {
        self.context.utils_class_name = name.into();
    }

    /// Enable/disable generation of the utils class.
    pub fn set_utils_enabled(&mut self, enabled: bool) {
        self.utils_enabled = enabled;
    }

    /// Add constant definition.
    pub fn add_const<T: Display>(&mut self, ty: &str, name: &str, value: T) {
        self.custom_consts.push(format!(
            "public const {} {} = {};",
            ty,
            name.to_pascal_case(),
            value
        ));
    }

    /// Clears the current filter and sets the filter mode.
    pub fn reset_filter(&mut self, filter_mode: FilterMode) {
        self.filter.clear();
        self.filter_mode = filter_mode;
    }

    /// Add the identifier to the filter.
    /// If the filter mode is `Blacklist` (the default), the identifiers in the
    /// filter are ignored.
    /// If it is `Whitelist`, the identifiers not in the filter are ignored.
    pub fn filter<T: Into<String>>(&mut self, ident: T) {
        let _ = self.filter.insert(ident.into());
    }

    /// Do not generate wrapper function for the given function.
    pub fn blacklist_wrapper_function<T: Into<String>>(&mut self, ident: T) {
        let _ = self.wrapper_function_blacklist.insert(ident.into());
    }

    pub fn reset_wrapper_function_blacklist(&mut self) {
        self.wrapper_function_blacklist.clear();
    }

    fn resolve_aliases(&mut self) {
        for snippet in &mut self.consts {
            resolve_alias(&self.aliases, &mut snippet.item.ty);
        }

        for snippet in &mut self.structs {
            if let Some(&Type::User(ref name)) = lookup_alias(&self.aliases, &snippet.name) {
                snippet.name = name.clone();
            }

            for field in &mut snippet.item.fields {
                resolve_alias(&self.aliases, &mut field.ty);
            }
        }

        for snippet in &mut self.functions {
            resolve_alias(&self.aliases, &mut snippet.item.output);

            for &mut (_, ref mut ty) in &mut snippet.item.inputs {
                resolve_alias(&self.aliases, ty)
            }
        }
    }

    fn resolve_native_types(&mut self) {
        let mut run = true;
        while run {
            run = false;

            for snippet in &self.structs {
                // If the struct is already marked as native, proceed to the next one.
                if self.context.is_native_name(&snippet.name) {
                    continue;
                }

                // Otherwise, check it one of its fields is native, and if it is,
                // mark the struct as native and reprocess the whole thing again,
                // to detect structs with newly identified native fields.
                if snippet.item.fields.iter().any(|field| {
                    field.ty.is_dynamic_array() || self.context.is_native_type(&field.ty)
                })
                {
                    let _ = self.context.native_types.insert(snippet.name.clone());
                    run = true;
                }
            }
        }
    }

    fn is_ignored(&self, ident: &str) -> bool {
        match self.filter_mode {
            FilterMode::Blacklist => self.filter.contains(ident),
            FilterMode::Whitelist => !self.filter.contains(ident),
        }
    }

    fn is_interface_function(&self, name: &str, item: &Function) -> bool {
        !self.wrapper_function_blacklist.contains(name) && num_callbacks(&item.inputs) <= 1
    }
}

impl Lang for LangCSharp {
    fn parse_ty(&mut self, item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        let name = item.ident.name.as_str();
        if self.is_ignored(&name) {
            return Ok(());
        }

        if let ast::ItemKind::Ty(ref ty, ref generics) = item.node {
            if generics.is_parameterized() {
                println!("parameterized type aliases not supported. Skipping.");
                return Ok(());
            }

            let ty = transform_type(ty).ok_or_else(|| {
                Error {
                    level: Level::Error,
                    span: Some(ty.span),
                    message: format!(
                        "bindgen can not handle the type `{}`",
                        pprust::ty_to_string(ty)
                    ),
                }
            })?;

            self.aliases.insert(name.to_string(), ty);
        }

        Ok(())
    }

    fn parse_const(&mut self, item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        let name = item.ident.name.as_str();
        if self.is_ignored(&name) {
            return Ok(());
        }

        let docs = common::parse_attr(&item.attrs, |_| true, retrieve_docstring).1;

        if let ast::ItemKind::Const(ref ty, ref expr) = item.node {
            let item = transform_const(ty, expr).ok_or_else(|| {
                Error {
                    level: Level::Error,
                    span: Some(expr.span),
                    message: format!(
                        "bindgen can not handle constant {}",
                        pprust::item_to_string(item)
                    ),
                }
            })?;
            let name = name.to_string();

            self.consts.push(Snippet { docs, name, item });
        }

        Ok(())
    }

    fn parse_enum(&mut self, item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        let name = item.ident.name.as_str();
        if self.is_ignored(&name) {
            return Ok(());
        }

        let (repr_c, docs) =
            common::parse_attr(&item.attrs, common::check_repr_c, retrieve_docstring);

        // If it's not #[repr(C)] ignore it.
        if !repr_c {
            return Ok(());
        }

        if let ast::ItemKind::Enum(ast::EnumDef { ref variants }, ref generics) = item.node {
            if generics.is_parameterized() {
                return Err(unsupported_generics_error(item, "enums"));
            }

            let item = transform_enum(variants).ok_or_else(|| {
                Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: format!(
                        "bindgen can not handle enum {}",
                        pprust::item_to_string(item)
                    ),
                }
            })?;
            let name = name.to_string();

            self.enums.push(Snippet { docs, name, item });
        }

        Ok(())
    }

    fn parse_struct(&mut self, item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        let name = item.ident.name.as_str();
        if self.is_ignored(&name) {
            return Ok(());
        }

        let (repr_c, docs) =
            common::parse_attr(&item.attrs, common::check_repr_c, retrieve_docstring);

        // If it's not #[repr(C)] ignore it.
        if !repr_c {
            return Ok(());
        }

        if let ast::ItemKind::Struct(ref variants, ref generics) = item.node {
            if generics.is_parameterized() {
                return Err(unsupported_generics_error(item, "structs"));
            }

            if !variants.is_struct() {
                return Err(Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: format!("bindgen can not handle unit or tuple structs ({})", name),
                });
            }

            let item = transform_struct(variants.fields()).ok_or_else(|| {
                Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: format!(
                        "bindgen can not handle struct {}",
                        pprust::item_to_string(item)
                    ),
                }
            })?;
            let name = name.to_string();
            self.structs.push(Snippet { docs, name, item });
            self.resolve_native_types();
        }

        Ok(())
    }

    fn parse_fn(&mut self, item: &ast::Item, _outputs: &mut Outputs) -> Result<(), Error> {
        let name = item.ident.name.as_str();
        if self.is_ignored(&name) {
            return Ok(());
        }

        let (no_mangle, docs) =
            common::parse_attr(&item.attrs, common::check_no_mangle, retrieve_docstring);

        // Ignore function without #[no_mangle].
        if !no_mangle {
            return Ok(());
        }

        if let ast::ItemKind::Fn(ref fn_decl, unsafety, ref constness, abi, ref generics, _) =
            item.node
        {
            if !common::is_extern(abi) {
                return Ok(());
            }

            if generics.is_parameterized() {
                return Err(unsupported_generics_error(item, "extern functions"));
            }

            let function = transform_function(&fn_decl).ok_or_else(|| {
                let string =
                    pprust::fun_to_string(fn_decl, unsafety, constness.node, item.ident, generics);

                Error {
                    level: Level::Error,
                    span: Some(item.span),
                    message: format!("bindgen can not handle function {}", string),
                }
            })?;

            self.functions.push(Snippet {
                docs,
                name: name.to_string(),
                item: function,
            });
        }

        Ok(())
    }

    fn finalise_output(&mut self, outputs: &mut Outputs) -> Result<(), Error> {
        self.resolve_aliases();

        if !self.functions.is_empty() {
            // Functions
            let mut writer = IndentedWriter::new(INDENT_WIDTH);

            emit!(writer, "using System;\n");
            emit!(writer, "using System.Collections.Generic;\n");
            emit!(writer, "using System.Runtime.InteropServices;\n");
            emit!(writer, "using System.Threading.Tasks;\n\n");
            emit!(writer, "namespace {} {{\n", self.context.namespace);
            writer.indent();

            emit!(
                writer,
                "public partial class {} : I{} {{\n",
                self.context.class_name,
                self.context.class_name
            );
            writer.indent();

            // Define constant with the native library name, to be used in
            // the [DllImport] attributes.
            emit!(writer, "#if __IOS__\n");
            emit!(writer, "internal const string DllName = \"__Internal\";\n");
            emit!(writer, "#else\n");
            emit!(
                writer,
                "internal const string DllName = \"{}\";\n",
                self.context.lib_name
            );
            emit!(writer, "#endif\n\n");

            for snippet in &self.functions {
                emit_docs(&mut writer, &self.context, &snippet.docs);
                if self.is_interface_function(&snippet.name, &snippet.item) {
                    emit_wrapper_function(&mut writer, &self.context, &snippet.name, &snippet.item);
                }
                emit_function_extern_decl(&mut writer, &self.context, &snippet.name, &snippet.item);
            }

            // Callback delegates and wrappers.
            {
                let callbacks = collect_callbacks(&self.functions);
                if !callbacks.is_empty() {
                    for (callback, single) in callbacks {
                        emit_callback_delegate(&mut writer, &self.context, callback);

                        if single {
                            emit_callback_wrapper(&mut writer, &self.context, callback);
                        }
                    }
                }
            }

            writer.unindent();
            emit!(writer, "}}\n");

            writer.unindent();
            emit!(writer, "}}\n");

            outputs.insert(
                PathBuf::from(format!("{}.cs", self.context.class_name)),
                writer.into_inner(),
            );

            // Interface
            let functions: Vec<_> = mem::replace(&mut self.functions, Vec::new());
            let mut functions = functions
                .into_iter()
                .filter(|snippet| {
                    self.is_interface_function(&snippet.name, &snippet.item)
                })
                .peekable();

            if functions.peek().is_some() {
                let mut writer = IndentedWriter::new(INDENT_WIDTH);

                emit!(writer, "using System;\n");
                emit!(writer, "using System.Collections.Generic;\n");
                emit!(writer, "using System.Runtime.InteropServices;\n");
                emit!(writer, "using System.Threading.Tasks;\n\n");
                emit!(writer, "namespace {} {{\n", self.context.namespace);
                writer.indent();

                emit!(
                    writer,
                    "public partial interface I{} {{\n",
                    self.context.class_name
                );
                writer.indent();

                for snippet in functions {
                    if num_callbacks(&snippet.item.inputs) <= 1 {
                        emit_wrapper_function_decl(
                            &mut writer,
                            &self.context,
                            "",
                            &snippet.name,
                            &snippet.item,
                        );
                        emit!(writer, ";\n");
                    }
                }

                writer.unindent();
                emit!(writer, "}}\n");

                writer.unindent();
                emit!(writer, "}}\n");

                outputs.insert(
                    PathBuf::from(format!("I{}.cs", self.context.class_name)),
                    writer.into_inner(),
                );
            }
        }

        // Constants
        if !self.consts.is_empty() || !self.custom_consts.is_empty() {
            let mut writer = IndentedWriter::new(INDENT_WIDTH);
            emit!(writer, "using System;\n\n");
            emit!(writer, "namespace {} {{\n", self.context.namespace);
            writer.indent();

            emit!(
                writer,
                "public static class {} {{\n",
                self.context.consts_class_name
            );
            writer.indent();

            for snippet in self.consts.drain(..) {
                emit_docs(&mut writer, &self.context, &snippet.docs);
                emit_const(&mut writer, &self.context, &snippet.name, &snippet.item);
            }

            if !self.custom_consts.is_empty() {
                for decl in self.custom_consts.drain(..) {
                    emit!(writer, "{}\n", decl);
                }
            }

            writer.unindent();
            emit!(writer, "}}\n");

            writer.unindent();
            emit!(writer, "}}\n");

            outputs.insert(
                PathBuf::from(format!("{}.cs", self.context.consts_class_name)),
                writer.into_inner(),
            );

        }

        // Types
        if self.types_enabled && (!self.enums.is_empty() || !self.structs.is_empty()) {
            let mut writer = IndentedWriter::new(INDENT_WIDTH);

            emit!(writer, "using System;\n");
            emit!(writer, "using System.Collections.Generic;\n");
            emit!(writer, "using System.Runtime.InteropServices;\n");
            emit!(writer, "using JetBrains.Annotations;\n\n");

            emit!(writer, "namespace {} {{\n", self.context.namespace);
            writer.indent();

            // Enums
            for snippet in self.enums.drain(..) {
                emit_docs(&mut writer, &self.context, &snippet.docs);
                emit_enum(&mut writer, &self.context, &snippet.name, &snippet.item);
            }

            // Structs
            for snippet in &self.structs {
                emit_docs(&mut writer, &self.context, &snippet.docs);

                if self.context.is_native_name(&snippet.name) {
                    emit_wrapper_struct(&mut writer, &self.context, &snippet.name, &snippet.item);
                    emit_native_struct(&mut writer, &self.context, &snippet.name, &snippet.item);
                } else {
                    emit_normal_struct(&mut writer, &self.context, &snippet.name, &snippet.item);
                }
            }

            writer.unindent();
            emit!(writer, "}}\n");

            outputs.insert(
                PathBuf::from(format!("{}.cs", self.context.types_file_name)),
                writer.into_inner(),
            );
        }

        // Utilities
        if self.utils_enabled {
            let mut writer = IndentedWriter::new(INDENT_WIDTH);
            emit_utilities(&mut writer, &self.context);

            outputs.insert(
                PathBuf::from(format!("{}.cs", self.context.utils_class_name)),
                writer.into_inner(),
            );
        }

        // Other cleanup.
        self.context.opaque_types.clear();
        self.context.native_types.clear();

        Ok(())
    }
}

fn resolve_alias(aliases: &HashMap<String, Type>, new_ty: &mut Type) {
    let mut orig_new_ty = mem::replace(new_ty, Type::Unit);

    match orig_new_ty {
        Type::User(ref name) => {
            if let Some(old_ty) = lookup_alias(aliases, name) {
                *new_ty = old_ty.clone();
                return;
            }
        }
        Type::Pointer(ref mut ty) => {
            resolve_alias(aliases, ty);
        }
        Type::Array(ref mut ty, _) => {
            resolve_alias(aliases, ty);
        }
        Type::Function(ref mut fun) => {
            resolve_alias(aliases, &mut fun.output);
            for &mut (_, ref mut input) in &mut fun.inputs {
                resolve_alias(aliases, input);
            }
        }
        _ => (),
    }

    mem::replace(new_ty, orig_new_ty);
}

fn lookup_alias<'a>(aliases: &'a HashMap<String, Type>, name: &str) -> Option<&'a Type> {
    if let Some(ty) = aliases.get(name) {
        if let Type::User(ref name) = *ty {
            Some(lookup_alias(aliases, name).unwrap_or(ty))
        } else {
            Some(ty)
        }
    } else {
        None
    }
}

fn collect_callbacks(functions: &[Snippet<Function>]) -> Vec<(&Function, bool)> {
    let mut stash = BTreeMap::new();

    for snippet in functions {
        let callbacks = extract_callbacks(&snippet.item.inputs);
        let count = callbacks.len();

        for callback in callbacks {
            let name = callback_wrapper_name(callback);

            match stash.entry(name) {
                Entry::Vacant(entry) => {
                    let _ = entry.insert((callback, count == 1));
                }
                Entry::Occupied(mut entry) => {
                    if count == 1 {
                        entry.get_mut().1 = true;
                    }
                }
            }
        }
    }

    stash.into_iter().map(|(_, entry)| entry).collect()
}

fn callback_wrapper_name(callback: &Function) -> String {
    let mut writer = IndentedWriter::new(INDENT_WIDTH);
    emit_callback_wrapper_name(&mut writer, callback);
    writer.into_inner()
}

fn unsupported_generics_error(item: &ast::Item, name: &str) -> Error {
    Error {
        level: Level::Error,
        span: Some(item.span),
        message: format!("bindgen can not handle parameterized {}", name),
    }
}
