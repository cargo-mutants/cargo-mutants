// Copyright 2021 Martin Pool

//! Visit the abstract syntax tree and discover things to mutate.
//!
//! Knowledge of the syn API is localized here.

use quote::ToTokens;
use syn::visit::Visit;
use syn::Attribute;
use syn::ItemFn;

use crate::mutate::{Mutation, MutationOp};
use crate::source::SourceFile;

/// `syn` visitor that recursively traverses the syntax tree, accumulating places that could be mutated.
pub struct DiscoveryVisitor<'sf> {
    /// All the mutations generated by visiting the file.
    pub mutations: Vec<Mutation>,

    /// The file being visited.
    source_file: &'sf SourceFile,

    /// The stack of namespaces we're currently inside.
    namespace_stack: Vec<String>,
}

impl<'sf> DiscoveryVisitor<'sf> {
    pub fn new(source_file: &'sf SourceFile) -> DiscoveryVisitor<'sf> {
        DiscoveryVisitor {
            source_file,
            mutations: Vec::new(),
            namespace_stack: Vec::new(),
        }
    }

    fn collect_fn_mutations(
        &mut self,
        ident: &syn::Ident,
        return_type: &syn::ReturnType,
        span: &proc_macro2::Span,
    ) {
        self.in_namespace(&ident.to_string(), |v| {
            let function_name = v.namespace_stack.join("::");
            let return_type_str = format!("{}", return_type.to_token_stream());
            for op in ops_for_return_type(return_type) {
                v.mutations.push(Mutation::new(
                    v.source_file.clone(),
                    op,
                    function_name.clone(),
                    return_type_str.clone(),
                    span.into(),
                ))
            }
        });
    }

    /// Call a function with a namespace pushed onto the stack.
    ///
    /// This is used when recursively descending into a namespace.
    fn in_namespace<F, T>(&mut self, name: &str, f: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        self.namespace_stack.push(name.to_owned());
        let r = f(self);
        assert_eq!(self.namespace_stack.pop().unwrap(), name);
        r
    }
}

impl<'ast, 'sf> Visit<'ast> for DiscoveryVisitor<'sf> {
    fn visit_item_fn(&mut self, i: &'ast ItemFn) {
        // TODO: Filter out more inapplicable fns.
        if attrs_excluded(&i.attrs) {
            return; // don't look inside it either
        }
        self.collect_fn_mutations(&i.sig.ident, &i.sig.output, &i.block.brace_token.span);
        self.in_namespace(&i.sig.ident.to_string(), |v| {
            syn::visit::visit_item_fn(v, i);
        });
    }

    /// Visit `impl Foo { ...}` or `impl Debug for Foo { ... }`.
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if attrs_excluded(&i.attrs) {
            return;
        }
        // Make an approximately-right namespace.
        let name = type_name_string(&i.self_ty);
        self.in_namespace(&name, |v| syn::visit::visit_item_impl(v, i));
    }

    /// Visit `fn foo()` within an `impl`.
    fn visit_impl_item_method(&mut self, i: &'ast syn::ImplItemMethod) {
        if attrs_excluded(&i.attrs) {
            return;
        }
        self.collect_fn_mutations(&i.sig.ident, &i.sig.output, &i.block.brace_token.span);
        self.in_namespace(&i.sig.ident.to_string(), |v| {
            syn::visit::visit_impl_item_method(v, i)
        });
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !attrs_excluded(&node.attrs) {
            self.in_namespace(&node.ident.to_string(), |v| {
                syn::visit::visit_item_mod(v, node)
            });
        }
    }
}

fn ops_for_return_type(return_type: &syn::ReturnType) -> Vec<MutationOp> {
    let mut ops: Vec<MutationOp> = Vec::new();
    match return_type {
        syn::ReturnType::Default => ops.push(MutationOp::Unit),
        syn::ReturnType::Type(_rarrow, box_typ) => match &**box_typ {
            syn::Type::Path(syn::TypePath { path, .. }) => {
                // dbg!(&path);
                if path.is_ident("bool") {
                    ops.push(MutationOp::True);
                    ops.push(MutationOp::False);
                } else if path.is_ident("String") {
                    // TODO: Detect &str etc.
                    ops.push(MutationOp::EmptyString);
                    ops.push(MutationOp::Xyzzy);
                } else if path_is_result(path) {
                    // TODO: Try this for any path ending in "Result".
                    // TODO: Recursively generate for types inside the Ok side of the Result.
                    ops.push(MutationOp::OkDefault);
                } else {
                    ops.push(MutationOp::Default)
                }
            }
            _ => ops.push(MutationOp::Default),
        },
    }
    ops
}

fn type_name_string(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => {
            if let Some(ident) = p.path.get_ident() {
                format!("{}", ident)
            } else {
                // TODO: Something better here?
                "<??>".into()
            }
        }
        _ => "<??>".into(),
    }
}

fn path_is_result(path: &syn::Path) -> bool {
    path.segments
        .last()
        .map(|segment| segment.ident == "Result")
        .unwrap_or_default()
}

/// True if any of the attrs indicate that we should skip this node and everything inside it.
fn attrs_excluded(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr_is_cfg_test(attr) || attr_is_test(attr) || attr_is_mutants_skip(attr))
}

/// True if the attribute is `#[cfg(test)]`.
fn attr_is_cfg_test(attr: &Attribute) -> bool {
    if !attr.path.is_ident("cfg") {
        return false;
    }
    if let syn::Meta::List(meta_list) = attr.parse_meta().unwrap() {
        // We should have already checked this above, but to make sure:
        assert!(meta_list.path.is_ident("cfg"));
        for nested_meta in meta_list.nested {
            if let syn::NestedMeta::Meta(syn::Meta::Path(cfg_path)) = nested_meta {
                if cfg_path.is_ident("test") {
                    return true;
                }
            }
        }
    }
    false
}

/// True if the attribute is `#[test]`.
fn attr_is_test(attr: &Attribute) -> bool {
    attr.path.is_ident("test")
}

/// True if the attribute is `#[mutants::skip]`.
fn attr_is_mutants_skip(attr: &Attribute) -> bool {
    attr.path
        .segments
        .iter()
        .map(|ps| &ps.ident)
        .eq(["mutants", "skip"].iter())
}

#[cfg(test)]
mod test {
    #[test]
    fn path_is_result() {
        let path: syn::Path = syn::parse_quote! { Result<(), ()> };
        assert!(super::path_is_result(&path));
    }
}
