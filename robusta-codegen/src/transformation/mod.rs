use std::collections::HashSet;
use std::iter;
use std::str::FromStr;

use darling::FromMeta;
use proc_macro2::{Ident, TokenStream};
use proc_macro_error::emit_error;
use proc_macro_error::emit_warning;
use quote::{quote_spanned, ToTokens};
use syn::{Attribute, FnArg, GenericArgument, GenericParam, Generics, ImplItemMethod, Item, ItemImpl, ItemMod, ItemStruct, Lifetime, LifetimeDef, Lit, parse_quote, Pat, Path, PathArguments, PatIdent, PatType, ReturnType, Signature, Type, TypePath, TypeReference, Visibility, Expr};
use syn::fold::Fold;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::Token;
use syn::visit::Visit;

use exported::{CallType, ExportedMethodTransformer, ImplExportVisitor};
use imported::ImportedMethodTransformer;

use crate::utils::{canonicalize_path, unique_ident, is_self_method};
use crate::validation::JNIBridgeModule;
use std::iter::FromIterator;

mod imported;
mod exported;

#[derive(Copy, Clone)]
pub(crate) enum ImplItemType {
    Exported,
    Imported,
    Unexported,
}

pub(crate) struct ModTransformer {
    module: JNIBridgeModule
}

impl ModTransformer {
    pub(crate) fn new(module: JNIBridgeModule) -> Self {
        ModTransformer {
            module
        }
    }

    pub(crate) fn transform_module(&mut self) -> TokenStream {
        let mut module_decl = self.module.module_decl.clone();
        if let Some((brace, mut items)) = module_decl.content {
            let jni_path_prefix = if cfg!(feature = "no_jni") {
                ""
            } else {
                "::robusta_jni"
            };

            let mut items_with_use: Vec<Item> = vec![
                syn::parse2(TokenStream::from_str("use std::convert::TryInto;").unwrap()).unwrap(),
                parse_quote! { use ::robusta_jni::convert::{FromJavaValue, IntoJavaValue, TryFromJavaValue, TryIntoJavaValue, JValueWrapper, JavaValue}; },
                syn::parse2(TokenStream::from_str(&format!("use {}::jni::objects::{{JClass, JValue}};", jni_path_prefix)).unwrap()).unwrap()
            ];
            items_with_use.append(&mut items);

            module_decl.content = Some((brace, items_with_use));
        }

        self.fold_item_mod(module_decl).into_token_stream()
    }

    /// If the impl block is a standard impl block for a type, makes every exported fn a freestanding one
    fn transform_item_impl(&mut self, node: ItemImpl) -> TokenStream {
        let mut impl_export_visitor = ImplExportVisitor::default();
        impl_export_visitor.visit_item_impl(&node);

        let (preserved_items, transformed_items) = if let Type::Path(p) = &*node.self_ty {
            let canonical_path = canonicalize_path(&p.path);
            let struct_name = canonical_path.to_token_stream().to_string().replace(" ", ""); // TODO: Replace String-based struct name matching with something more robust
            let struct_package = self.module.package_map.get(&struct_name).cloned().flatten();

            // TODO: Is this ok?
            if let None = struct_package {
                emit_warning!(p.path, "can't find package for struct `{}`", struct_name);
                return node.to_token_stream();
            }

            let mut exported_fns_transformer = ExportedMethodTransformer { struct_type: p.path.clone(), struct_name: struct_name.clone(), package: struct_package.clone() };
            let mut imported_fns_transformer = ImportedMethodTransformer { struct_name, package: struct_package };
            let mut impl_cleaner = ImplCleaner;

            let preserved = impl_export_visitor.items.iter()
                .map(|(i, t)| {
                    let item = (*i).clone();
                    match t {
                        ImplItemType::Exported => impl_cleaner.fold_impl_item(item),
                        ImplItemType::Imported => imported_fns_transformer.fold_impl_item(impl_cleaner.fold_impl_item(item)),
                        ImplItemType::Unexported => item,
                    }
                })
                .collect();

            let transformed = impl_export_visitor.items.into_iter()
                .filter_map(|(i, t)| {
                    match t {
                        ImplItemType::Exported => Some(i),
                        _ => None
                    }
                }).cloned().map(|i| exported_fns_transformer.fold_impl_item(i)).collect();

            (preserved, transformed)
        } else {
            (node.items, Vec::new())
        };

        let preserved_impl = ItemImpl {
            attrs: node.attrs.into_iter().map(|a| self.fold_attribute(a)).collect(),
            generics: self.fold_generics(node.generics),
            self_ty: Box::new(self.fold_type(*node.self_ty)),
            items: preserved_items.into_iter().map(|i| self.fold_impl_item(i)).collect(),
            ..node
        };

        transformed_items.iter()
            .map(|i| i.to_token_stream())
            .fold(preserved_impl.into_token_stream(), |item, mut stream| {
                item.to_tokens(&mut stream);
                stream
            })
    }
}

impl Fold for ModTransformer {
    fn fold_item(&mut self, node: Item) -> Item {
        match node {
            Item::Const(c) => Item::Const(self.fold_item_const(c)),
            Item::Enum(e) => Item::Enum(self.fold_item_enum(e)),
            Item::ExternCrate(c) => Item::ExternCrate(self.fold_item_extern_crate(c)),
            Item::Fn(f) => Item::Fn(self.fold_item_fn(f)),
            Item::ForeignMod(m) => Item::ForeignMod(self.fold_item_foreign_mod(m)),
            Item::Impl(i) => {
                Item::Verbatim(self.transform_item_impl(i))
            }
            Item::Macro(m) => Item::Macro(self.fold_item_macro(m)),
            Item::Macro2(m) => Item::Macro2(self.fold_item_macro2(m)),
            Item::Mod(m) => Item::Mod(self.fold_item_mod(m)),
            Item::Static(s) => Item::Static(self.fold_item_static(s)),
            Item::Struct(s) => Item::Struct(self.fold_item_struct(s)),
            Item::Trait(t) => Item::Trait(self.fold_item_trait(t)),
            Item::TraitAlias(t) => Item::TraitAlias(self.fold_item_trait_alias(t)),
            Item::Type(t) => Item::Type(self.fold_item_type(t)),
            Item::Union(u) => Item::Union(self.fold_item_union(u)),
            Item::Use(u) => Item::Use(self.fold_item_use(u)),
            Item::Verbatim(_) => node,
            _ => node,
        }
    }

    fn fold_item_mod(&mut self, mut node: ItemMod) -> ItemMod {
        let allow_non_snake_case: Attribute = parse_quote! { #![allow(non_snake_case)] };
        let allow_unused: Attribute = parse_quote! { #![allow(unused)] };

        node.attrs.extend_from_slice(&[allow_non_snake_case, allow_unused]);

        ItemMod {
            attrs: node.attrs,
            vis: self.fold_visibility(node.vis),
            mod_token: node.mod_token,
            ident: self.fold_ident(node.ident),
            content: node.content.map(|(brace, items)| (brace, items.into_iter().map(|i| self.fold_item(i)).collect())),
            semi: node.semi,
        }
    }

    fn fold_item_struct(&mut self, node: ItemStruct) -> ItemStruct {
        let discarded_known_attributes = {
            let mut known = HashSet::new();
            known.insert("package");
            known
        };

        let struct_attributes = {
            let attributes = node.attrs;
            attributes.into_iter().filter(|a| !discarded_known_attributes.contains(&a.path.to_token_stream().to_string().as_str())).collect()
        };

        ItemStruct {
            attrs: struct_attributes,
            vis: node.vis,
            struct_token: node.struct_token,
            ident: node.ident,
            generics: self.fold_generics(node.generics),
            fields: self.fold_fields(node.fields),
            semi_token: node.semi_token,
        }
    }
}

#[derive(Clone)]
struct JavaPath(String);

impl FromMeta for JavaPath {
    fn from_value(value: &Lit) -> darling::Result<Self> {
        use darling::Error;

        if let Lit::Str(literal) = value {
            let path = literal.value();
            if path.contains('-') {
                Err(Error::custom("invalid path: packages and classes cannot contain dashes"))
            } else {
                let tokens = TokenStream::from_str(&path).map_err(|_| Error::custom("cannot create token stream for java path parsing"))?;
                let _parsed: Punctuated<Ident, Token![.]> = Punctuated::<Ident, Token![.]>::parse_separated_nonempty.parse(tokens.into()).map_err(|e| Error::custom(format!("cannot parse java path ({})", e)))?;

                Ok(JavaPath(path))
            }
        } else {
            Err(Error::custom("invalid type"))
        }
    }
}

struct AttributeFilter<'ast> {
    pub whitelist: HashSet<Path>,
    pub filtered_attributes: Vec<&'ast Attribute>,
}

impl<'ast> AttributeFilter<'ast> {
    fn with_whitelist(whitelist: HashSet<Path>) -> Self {
        AttributeFilter {
            whitelist,
            filtered_attributes: Vec::new(),
        }
    }
}

impl<'ast> Visit<'ast> for AttributeFilter<'ast> {
    fn visit_attribute(&mut self, attribute: &'ast Attribute) {
        if self.whitelist.contains(&attribute.path) {
            self.filtered_attributes.push(attribute);
        }
    }
}

struct ImplCleaner;

impl Fold for ImplCleaner {
    fn fold_impl_item_method(&mut self, mut node: ImplItemMethod) -> ImplItemMethod {
        let abi = node.sig.abi.as_ref().and_then(|l| l.name.as_ref().map(|n| n.value()));

        match (&node.vis, &abi.as_deref()) {
            (Visibility::Public(_), Some("jni")) => {
                node.sig.abi = None;
                node.attrs = node.attrs.into_iter().filter(|a| a.path.get_ident().map_or(false, |i| i != "call_type")).collect();

                node
            }
            (_, _) => node
        }
    }
}

struct FreestandingTransformer {
    struct_type: Path,
    struct_name: String,
    fn_name: String,
}

impl FreestandingTransformer {
    fn new(struct_type: Path, struct_name: String, fn_name: String) -> Self {
        FreestandingTransformer {
            struct_type,
            struct_name,
            fn_name,
        }
    }
}

impl Fold for FreestandingTransformer {
    fn fold_fn_arg(&mut self, arg: FnArg) -> FnArg {
        match arg {
            FnArg::Receiver(r) => {
                let receiver_span = r.span();

                let has_env_lifetime = self.struct_type.segments.iter().any(|s| {
                    if let PathArguments::AngleBracketed(a) = &s.arguments {
                        a.args.iter()
                            .filter_map(|g| {
                                match g {
                                    GenericArgument::Lifetime(l) => Some(l),
                                    _ => None
                                }
                            })
                            .any(|l| {
                                l.ident.to_string() == "env"
                            })
                    } else {
                        false
                    }
                });

                if !has_env_lifetime {
                    emit_error!(self.struct_type, "must have one `'env` lifetime in impl to support self methods when using lifetime-parametrized struct");
                }

                let self_type = match r.reference.clone() {
                    Some((and_token, lifetime)) => {
                        Type::Reference(TypeReference {
                            and_token,
                            lifetime,
                            mutability: r.mutability,
                            elem: Box::new(Type::Path(TypePath {
                                qself: None,
                                path: self.struct_type.clone()
                            })),
                        })
                    }

                    None => Type::Path(TypePath {
                        qself: None,
                        path: self.struct_type.clone()
                    })
                };

                FnArg::Typed(PatType {
                    attrs: r.attrs,
                    pat: Box::new(Pat::Ident(PatIdent {
                        attrs: vec![],
                        by_ref: None,
                        mutability: None,
                        ident: unique_ident(&format!("receiver_{}_{}", self.struct_name, self.fn_name), receiver_span),
                        subpat: None,
                    })),
                    colon_token: Token![:](receiver_span),
                    ty: Box::new(self_type),
                })
            }

            FnArg::Typed(t) => {
                match &*t.pat {
                    Pat::Ident(ident) if ident.ident == "self" => {
                        let pat_span = t.span();
                        FnArg::Typed(PatType {
                            attrs: t.attrs,
                            pat: Box::new(Pat::Ident(PatIdent {
                                attrs: ident.attrs.clone(),
                                by_ref: ident.by_ref,
                                mutability: ident.mutability,
                                ident: unique_ident(&format!("receiver_{}_{}", self.struct_name, self.fn_name), pat_span),
                                subpat: ident.subpat.clone(),
                            })),
                            colon_token: t.colon_token,
                            ty: t.ty.clone(),
                        })
                    }

                    _ => FnArg::Typed(t)
                }
            }
        }
    }
}

struct JNISignatureTransformer {
    struct_type: Path,
    struct_name: String,
    fn_name: String,
    call_type: CallType,
}

impl JNISignatureTransformer {
    fn new(struct_type: Path, struct_name: String, fn_name: String, call_type: CallType) -> Self {
        JNISignatureTransformer {
            struct_type,
            struct_name,
            fn_name,
            call_type,
        }
    }

    fn transform_generics(&mut self, mut generics: Generics, self_method: bool) -> Generics {
        if self_method {
            let struct_lifetimes: Vec<&Lifetime> = self.struct_type.segments.iter().filter_map(|s| {
                match &s.arguments {
                    PathArguments::AngleBracketed(a) => {
                        let segment_lifetimes: Vec<_> = a.args.iter().filter_map(|a| {
                            match a {
                                GenericArgument::Lifetime(l) if l.ident.to_string() != "env" => {
                                    Some(l)
                                }
                                _ => None
                            }
                        }).collect();

                        Some(segment_lifetimes)
                    }
                    _ => None
                }
            }).flatten().collect();

            struct_lifetimes.into_iter().for_each(|l| {
                generics.params.push(GenericParam::Lifetime(LifetimeDef {
                    attrs: vec![],
                    lifetime: l.clone(),
                    colon_token: None,
                    bounds: Default::default()
                }))
            });
        }

        generics.params.push(GenericParam::Lifetime(LifetimeDef {
            attrs: vec![],
            lifetime: Lifetime {
                apostrophe: generics.span(),
                ident: Ident::new("env", generics.span()),
            },
            colon_token: None,
            bounds: Default::default(),
        }));

        generics
    }
}

impl Fold for JNISignatureTransformer {
    fn fold_fn_arg(&mut self, arg: FnArg) -> FnArg {
        let mut freestanding_transformer = FreestandingTransformer::new(self.struct_type.clone(), self.struct_name.clone(), self.fn_name.clone());

        match freestanding_transformer.fold_fn_arg(arg) {
            FnArg::Receiver(_) => panic!("Bug -- please report to library author. Found receiver input after freestanding conversion"),
            FnArg::Typed(t) => {
                let original_input_type = t.ty;

                let jni_conversion_type: Type = match self.call_type {
                    CallType::Safe(_) => syn::parse2(quote_spanned! { original_input_type.span() => <#original_input_type as TryFromJavaValue<'env>>::Source }).unwrap(),
                    CallType::Unchecked { .. } => syn::parse2(quote_spanned! { original_input_type.span() => <#original_input_type as FromJavaValue<'env>>::Source }).unwrap(),
                };

                FnArg::Typed(PatType {
                    attrs: t.attrs,
                    pat: t.pat,
                    colon_token: t.colon_token,
                    ty: Box::new(jni_conversion_type),
                })
            }
        }
    }

    fn fold_return_type(&mut self, return_type: ReturnType) -> ReturnType {
        match return_type {
            ReturnType::Default => return_type,
            ReturnType::Type(ref arrow, ref rtype) => {
                match (&**rtype, self.call_type.clone()) {
                    (Type::Path(p), CallType::Unchecked { .. }) => {
                        ReturnType::Type(*arrow, syn::parse2(quote_spanned! { p.span() => <#p as IntoJavaValue<'env>>::Target }).unwrap())
                    }

                    (Type::Path(p), CallType::Safe(_)) => {
                        ReturnType::Type(*arrow, syn::parse2(quote_spanned! { p.span() => <#p as TryIntoJavaValue<'env>>::Target }).unwrap())
                    }

                    (Type::Reference(r), CallType::Unchecked { .. }) => {
                        ReturnType::Type(*arrow, syn::parse2(quote_spanned! { r.span() => <#r as IntoJavaValue<'env>>::Target }).unwrap())
                    }

                    (Type::Reference(r), CallType::Safe(_)) => {
                        ReturnType::Type(*arrow, syn::parse2(quote_spanned! { r.span() => <#r as TryIntoJavaValue<'env>>::Target }).unwrap())
                    }
                    _ => {
                        emit_error!(return_type, "Only type or type paths are permitted as type ascriptions in function params");
                        return_type
                    }
                }
            }
        }
    }

    fn fold_signature(&mut self, node: Signature) -> Signature {
        let self_method = is_self_method(&node);

        Signature {
            abi: node.abi.map(|a| self.fold_abi(a)),
            ident: self.fold_ident(node.ident),
            generics: self.transform_generics(node.generics, self_method),
            inputs: node.inputs.into_iter().map(|f| self.fold_fn_arg(f)).collect(),
            variadic: node.variadic.map(|v| self.fold_variadic(v)),
            output: self.fold_return_type(node.output),
            ..node
        }
    }
}

struct JNISignature {
    transformed_signature: Signature,
    call_type: CallType,
    struct_name: String,
    self_method: bool,
    env_arg: Option<FnArg>
}

impl JNISignature {
    fn new(signature: Signature, struct_type: Path, struct_name: String, call_type: CallType) -> JNISignature {
        let mut jni_signature_transformer = JNISignatureTransformer::new(struct_type.clone(), struct_name.clone(), signature.ident.to_string(), call_type.clone());

        let self_method = is_self_method(&signature);

        // Check whether second argument (first exluding self) is of type &JNIEnv, if so we don't transform it
        let possible_env_arg = if !self_method {
            signature.inputs.iter().next()
        } else {
            signature.inputs.iter().nth(1)
        };

        let has_explicit_env_arg = if let Some(FnArg::Typed(PatType { ty, ..})) = possible_env_arg {
            if let Type::Reference(TypeReference { elem, .. }) = &**ty {
                if let Type::Path(t) = &**elem {
                    let full_path: Path = parse_quote! { ::robusta_jni::jni::JNIEnv };
                    let imported_path: Path = parse_quote! { JNIEnv };
                    let canonicalized_type_path = canonicalize_path(&t.path);

                    canonicalized_type_path == imported_path || canonicalized_type_path == full_path
                } else { false }
            } else { false }
        } else { false };

        let (transformed_signature, env_arg): (Signature, Option<FnArg>) = if has_explicit_env_arg {
            let mut inner_signature = signature;

            let mut iter = inner_signature.inputs.into_iter();

            if self_method {
                let self_arg = iter.next();
                let env_arg = iter.next();

                inner_signature.inputs = iter::once(self_arg.unwrap()).chain(iter).collect();
                (inner_signature, env_arg)
            } else {
                let env_arg = iter.next();
                inner_signature.inputs = iter.collect();

                (inner_signature, env_arg)
            }
        } else {
            (signature, None)
        };

        let transformed_signature = jni_signature_transformer.fold_signature(transformed_signature);

        JNISignature {
            transformed_signature,
            call_type,
            struct_name,
            self_method,
            env_arg
        }

    }

    fn signature_call(&self) -> Expr {
        let method_call_inputs: Punctuated<Expr, Token![,]> = {
            let mut result: Vec<_> = self.transformed_signature.inputs.iter()
                .map(|arg| {
                    if let FnArg::Typed(PatType { pat, .. }) = arg {
                        if let Pat::Ident(PatIdent { ident, .. }) = &**pat {
                            let input_param: Expr = {
                                match self.call_type {
                                    CallType::Safe(_) => parse_quote! { TryFromJavaValue::try_from(#ident, &env)? },
                                    CallType::Unchecked { .. } => parse_quote! { FromJavaValue::from(#ident, &env) }
                                }
                            };
                            input_param
                        } else {
                            panic!("Bug -- please report to library author. Found non-ident FnArg pattern");
                        }
                    } else {
                        panic!("Bug -- please report to library author. Found receiver FnArg after freestanding transform");
                    }
                }).collect();

            if self.env_arg.is_some() {
                // because `self` is kept in the transformed JNI signature, if this is a `self` method we put `env` *after* self, otherwise the env parameter must be first
                let idx = if self.self_method { 1 } else { 0 };
                result.insert(idx, parse_quote!(&env));
            }

            Punctuated::from_iter(result.into_iter())
        };

        let signature = self.transformed_signature.span();
        let struct_name = Ident::new(&self.struct_name, signature);
        let method_name = self.transformed_signature.ident.clone();

        parse_quote! {
            #struct_name::#method_name(#method_call_inputs)
        }
    }

    fn transformed_signature(&self) -> &Signature {
        &self.transformed_signature
    }
}