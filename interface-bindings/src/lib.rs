// Copyright 2026  Pierre Krieger

//! Generates Rust bindings for an interface definition.
//!
//! See the documentation of [`interface_bindings!`].

extern crate alloc;
extern crate proc_macro;

use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString as _},
    vec::Vec,
};
use interface_parser::{
    Assignment, Group, GroupChoice, GroupEntry, GroupEntryKind, MemberKey, Message, Occurrence,
    Operator, Rule, RuleBody, Type, Type1, Type2, Value,
};
use proc_macro2::{Ident, TokenStream};
use quote::quote;

/// Generates Rust bindings for the interface definition found in the given `.cddl` file.
///
/// The path is relative to the directory containing the `Cargo.toml` of the crate that invokes
/// the macro. See the documentation of the `interface-parser` crate for the expected format.
///
/// The macro generates a module whose name is the snake_case version of the name of the file,
/// and that contains:
///
/// - One Rust type per rule of the document, whose name is the PascalCase version of the name of
///   the rule. Types that contain byte strings or text strings borrow them, and thus have a
///   lifetime parameter.
/// - One `emit_foo(request: &Foo)` function per message of the interface, where `foo` is the
///   name of the request or notification. If the message expects a response, the function
///   returns that response.
///
/// ```ignore
/// interface_bindings::interface_bindings!("interfaces/executor.cddl");
///
/// let response: executor::StartResponse = executor::emit_start_request(&request);
/// ```
///
/// Fails to compile if the file can't be read, if the interface definition is invalid, or if it
/// uses CDDL features that can't be converted to Rust types.
#[proc_macro]
pub fn interface_bindings(tokens: proc_macro::TokenStream) -> proc_macro::TokenStream {
    interface_bindings_common(tokens, true)
}

/// Same as [`interface_bindings!`], except that the `emit_foo` functions aren't generated.
///
/// The generated module only contains the type definitions.
///
/// ```ignore
/// interface_bindings::interface_bindings_definitions_only!("interfaces/executor.cddl");
/// ```
#[proc_macro]
pub fn interface_bindings_definitions_only(
    tokens: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    interface_bindings_common(tokens, false)
}

/// Common implementation of [`interface_bindings!`] and
/// [`interface_bindings_definitions_only!`].
///
/// If `with_emit_functions` is `false`, the `emit_foo` functions aren't generated.
fn interface_bindings_common(
    tokens: proc_macro::TokenStream,
    with_emit_functions: bool,
) -> proc_macro::TokenStream {
    let path_literal = syn::parse_macro_input!(tokens as syn::LitStr);

    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return syn::Error::new_spanned(&path_literal, "CARGO_MANIFEST_DIR isn't set")
            .to_compile_error()
            .into();
    };
    let path = std::path::Path::new(&manifest_dir).join(path_literal.value());

    let (Some(path_str), Some(file_stem)) = (
        path.to_str(),
        path.file_stem().and_then(|stem| stem.to_str()),
    ) else {
        return syn::Error::new_spanned(&path_literal, "path must be valid UTF-8")
            .to_compile_error()
            .into();
    };

    let module_name = match make_ident(&snake_case(file_stem)) {
        Ok(ident) => ident,
        Err(err) => {
            return syn::Error::new_spanned(&path_literal, err)
                .to_compile_error()
                .into();
        }
    };

    let source = match std::fs::read(&path) {
        Ok(source) => source,
        Err(err) => {
            return syn::Error::new_spanned(
                &path_literal,
                format!("failed to read `{}`: {err}", path.display()),
            )
            .to_compile_error()
            .into();
        }
    };

    let definition = match interface_parser::parse(&source) {
        Ok(definition) => definition,
        Err(err) => {
            let before = &source[..err.offset()];
            let line = before.iter().filter(|byte| **byte == b'\n').count() + 1;
            let column = before.len()
                - before
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |pos| pos + 1)
                + 1;
            return syn::Error::new_spanned(
                &path_literal,
                format!(
                    "failed to parse `{}`: {} at {line}:{column}",
                    path.display(),
                    err.kind()
                ),
            )
            .to_compile_error()
            .into();
        }
    };

    let content = match generate(&definition, with_emit_functions) {
        Ok(content) => content,
        Err(err) => {
            return syn::Error::new_spanned(
                &path_literal,
                format!(
                    "failed to generate bindings for `{}`: {err}",
                    path.display()
                ),
            )
            .to_compile_error()
            .into();
        }
    };

    quote! {
        pub mod #module_name {
            #![allow(dead_code)]

            extern crate alloc;

            // Makes Cargo rebuild the invoking crate whenever the interface definition changes.
            const _: &[u8] = include_bytes!(#path_str);

            #content
        }
    }
    .into()
}

/// Generates the content of the module corresponding to the given interface definition.
///
/// If `with_emit_functions` is `false`, the `emit_foo` functions aren't generated. The messages
/// are still validated and the types of their responses are still generated.
fn generate(
    definition: &interface_parser::InterfaceDefinition,
    with_emit_functions: bool,
) -> Result<TokenStream, String> {
    let root_name = definition.rules[0].name.as_str();

    let mut generator = Generator {
        rules: BTreeMap::new(),
        generated_rules: BTreeMap::new(),
        rules_in_progress: BTreeSet::new(),
        type_names: BTreeSet::new(),
        items: Vec::new(),
    };

    for rule in &definition.rules {
        // The root rule and its extensions are covered by `definition.messages`.
        if rule.name == root_name {
            continue;
        }
        if rule.assignment != Assignment::Define {
            return Err(format!(
                "extending rule `{}` with `/=` or `//=` isn't supported",
                rule.name
            ));
        }
        if !rule.generic_parameters.is_empty() {
            return Err(format!("generic rule `{}` isn't supported", rule.name));
        }
        generator.rules.insert(&rule.name, rule);
    }

    // Generate a type for every rule, even unused ones, in the order of the document.
    for rule in &definition.rules {
        if rule.name != root_name && matches!(rule.body, RuleBody::Type(_)) {
            generator.rule_type(&rule.name)?;
        }
    }

    let mut emit_function_names = BTreeSet::new();
    let mut emit_functions = Vec::with_capacity(definition.messages.len());
    for message in &definition.messages {
        let (request, response) = match message {
            Message::RequestResponse {
                request, response, ..
            } => (
                match &request.choices[..] {
                    [single] => Some(single),
                    _ => None,
                },
                Some(response),
            ),
            Message::Notification { ty, .. } => (Some(ty), None),
        };

        let Some(Type1 {
            target: Type2::Name {
                name: request_name, ..
            },
            operator: None,
            ..
        }) = request
        else {
            return Err(String::from(
                "every request and notification must be the name of a rule",
            ));
        };
        if !generator.rules.contains_key(request_name.as_str()) {
            return Err(format!(
                "request or notification `{request_name}` must be a rule defined in the document"
            ));
        }

        let request_ident = make_ident(&pascal_case(request_name))?;
        let request_type = if generator.rule_type(request_name)? {
            quote!(#request_ident<'_>)
        } else {
            quote!(#request_ident)
        };

        let function_name = format!("emit_{}", snake_case(request_name));
        let function_ident = make_ident(&function_name)?;
        if !emit_function_names.insert(function_name) {
            return Err(format!(
                "multiple messages generate a function named `{function_ident}`"
            ));
        }

        let (generics, return_type) = match response {
            Some(response) => {
                let (response_type, has_lifetime) =
                    generator.type_expr(response, &format!("{request_ident}Response"))?;
                (
                    if has_lifetime { quote!(<'a>) } else { quote!() },
                    quote!(-> #response_type),
                )
            }
            None => (quote!(), quote!()),
        };

        if with_emit_functions {
            emit_functions.push(quote! {
                pub fn #function_ident #generics (request: &#request_type) #return_type {
                    let _ = request;
                    unimplemented!()
                }
            });
        }
    }

    let items = generator.items;
    Ok(quote! {
        #(#items)*
        #(#emit_functions)*
    })
}

/// State of the generation of the Rust types.
struct Generator<'d> {
    /// All the rules of the document defined with `=`, except for the root rule, by name.
    rules: BTreeMap<&'d str, &'d Rule>,
    /// Type rules whose Rust type has been generated, and whether that type has a lifetime
    /// parameter.
    generated_rules: BTreeMap<&'d str, bool>,
    /// Rules that are currently being processed. Used to detect recursive rules.
    rules_in_progress: BTreeSet<&'d str>,
    /// Names of all the Rust types generated so far. Used to detect name collisions.
    type_names: BTreeSet<String>,
    /// Generated Rust items.
    items: Vec<TokenStream>,
}

impl<'d> Generator<'d> {
    /// Generates the Rust type corresponding to the given type rule, if not done yet.
    ///
    /// Returns `true` if the Rust type has a lifetime parameter.
    fn rule_type(&mut self, name: &'d str) -> Result<bool, String> {
        if let Some(has_lifetime) = self.generated_rules.get(name) {
            return Ok(*has_lifetime);
        }

        let Some(Rule {
            body: RuleBody::Type(ty),
            ..
        }) = self.rules.get(name).copied()
        else {
            // The parser has already verified that groups aren't used as types and that all
            // names are defined, so this can only be the root rule.
            return Err(format!("rule `{name}` can't be referenced"));
        };

        if !self.rules_in_progress.insert(name) {
            return Err(format!("recursive rule `{name}` isn't supported"));
        }
        let has_lifetime = self.define_type(&make_ident(&pascal_case(name))?, ty)?;
        self.rules_in_progress.remove(name);

        self.generated_rules.insert(name, has_lifetime);
        Ok(has_lifetime)
    }

    /// Generates a Rust type named `ident` corresponding to the given type.
    ///
    /// Returns `true` if the Rust type has a lifetime parameter.
    fn define_type(&mut self, ident: &Ident, ty: &'d Type) -> Result<bool, String> {
        if let [choice] = &ty.choices[..] {
            if choice.operator.is_none()
                && let Type2::Array(group) | Type2::Map(group) = &choice.target
            {
                let is_map = matches!(choice.target, Type2::Map(_));
                if !is_collection(group, is_map) {
                    return self.define_struct(ident, group, is_map);
                }
            }

            let (inner, has_lifetime) = self.type1_expr(choice, &format!("{ident}Inner"))?;
            let generics = if has_lifetime { quote!(<'a>) } else { quote!() };
            self.register_type(ident, quote!(pub struct #ident #generics (pub #inner);))?;
            return Ok(has_lifetime);
        }

        let mut variant_names = BTreeSet::new();
        let mut variants = Vec::with_capacity(ty.choices.len());
        let mut has_lifetime = false;
        for (index, choice) in ty.choices.iter().enumerate() {
            let variant_name = match choice {
                Type1 {
                    target: Type2::Name { name, .. },
                    operator: None,
                    ..
                } => pascal_case(name),
                _ => format!("Variant{index}"),
            };
            let variant_ident = make_ident(&variant_name)?;
            let (payload, payload_has_lifetime) =
                self.type1_expr(choice, &format!("{ident}{variant_name}"))?;
            if !variant_names.insert(variant_name) {
                return Err(format!(
                    "type `{ident}` has multiple variants named `{variant_ident}`"
                ));
            }
            has_lifetime |= payload_has_lifetime;
            variants.push(quote!(#variant_ident(#payload)));
        }

        let generics = if has_lifetime { quote!(<'a>) } else { quote!() };
        self.register_type(ident, quote!(pub enum #ident #generics { #(#variants),* }))?;
        Ok(has_lifetime)
    }

    /// Generates a Rust struct named `ident` whose fields are the entries of the given group.
    ///
    /// Returns `true` if the Rust type has a lifetime parameter.
    fn define_struct(
        &mut self,
        ident: &Ident,
        group: &'d Group,
        is_map: bool,
    ) -> Result<bool, String> {
        let [GroupChoice { entries }] = &group.choices[..] else {
            return Err(format!(
                "group choices (`//`) in `{ident}` aren't supported"
            ));
        };

        let mut members = Vec::with_capacity(entries.len());
        self.flatten_group_entries(entries, &mut members)?;

        let named = members.iter().all(|member| {
            matches!(
                member.kind,
                GroupEntryKind::Member {
                    key: Some(MemberKey::Bareword(_) | MemberKey::Value(Value::Text(_))),
                    ..
                }
            )
        });
        if is_map && !named {
            return Err(format!(
                "map `{ident}` must only contain members whose key is a name"
            ));
        }

        let mut field_names = BTreeSet::new();
        let mut fields = Vec::with_capacity(members.len());
        let mut has_lifetime = false;
        for (index, member) in members.iter().enumerate() {
            let GroupEntryKind::Member { key, ty } = &member.kind else {
                // `flatten_group_entries` only returns members.
                unreachable!()
            };

            let field_ident = match key {
                Some(MemberKey::Bareword(name) | MemberKey::Value(Value::Text(name))) if named => {
                    let field_ident = make_ident(&snake_case(name))?;
                    if !field_names.insert(field_ident.to_string()) {
                        return Err(format!(
                            "type `{ident}` has multiple fields named `{field_ident}`"
                        ));
                    }
                    Some(field_ident)
                }
                _ => None,
            };

            let context = match key {
                Some(MemberKey::Bareword(name) | MemberKey::Value(Value::Text(name))) if named => {
                    format!("{ident}{}", pascal_case(name))
                }
                _ => format!("{ident}Item{index}"),
            };
            let (mut field_type, field_has_lifetime) = self.type_expr(ty, &context)?;
            has_lifetime |= field_has_lifetime;

            match member.occurrence {
                None
                | Some(Occurrence {
                    min: 1,
                    max: Some(1),
                }) => {}
                Some(Occurrence {
                    min: 0,
                    max: Some(1),
                }) => {
                    field_type = quote!(::core::option::Option<#field_type>);
                }
                Some(_) => {
                    // TODO: make this borrow from the input instead of allocating
                    field_type = quote!(alloc::vec::Vec<#field_type>);
                }
            }

            fields.push(match field_ident {
                Some(field_ident) => quote!(pub #field_ident: #field_type),
                None => quote!(pub #field_type),
            });
        }

        let generics = if has_lifetime { quote!(<'a>) } else { quote!() };
        let item = if named {
            quote!(pub struct #ident #generics { #(#fields),* })
        } else {
            quote!(pub struct #ident #generics (#(#fields),*);)
        };
        self.register_type(ident, item)?;
        Ok(has_lifetime)
    }

    /// Pushes to `out` all the members found in `entries`, inlining references to groups.
    fn flatten_group_entries(
        &mut self,
        entries: &'d [GroupEntry],
        out: &mut Vec<&'d GroupEntry>,
    ) -> Result<(), String> {
        for entry in entries {
            match &entry.kind {
                GroupEntryKind::Member { .. } => out.push(entry),
                GroupEntryKind::GroupName { name, .. } => {
                    if entry.occurrence.is_some() {
                        return Err(format!(
                            "occurrence indicator on group `{name}` isn't supported"
                        ));
                    }
                    let Some(Rule {
                        body: RuleBody::GroupEntry(group_entry),
                        ..
                    }) = self.rules.get(name.as_str()).copied()
                    else {
                        // The parser has already verified that the name refers to a group.
                        return Err(format!("`{name}` isn't a group"));
                    };
                    if !self.rules_in_progress.insert(name) {
                        return Err(format!("recursive group `{name}` isn't supported"));
                    }
                    self.flatten_group_entries(core::slice::from_ref(group_entry), out)?;
                    self.rules_in_progress.remove(name.as_str());
                }
                GroupEntryKind::Inline(group) => {
                    let ([GroupChoice { entries }], None) = (&group.choices[..], entry.occurrence)
                    else {
                        return Err(String::from(
                            "group choices (`//`) and occurrence indicators on inline groups \
                            aren't supported",
                        ));
                    };
                    self.flatten_group_entries(entries, out)?;
                }
            }
        }

        Ok(())
    }

    /// Returns the Rust type corresponding to the given type, generating new Rust types if
    /// necessary. `context` is the name of the Rust type to generate if `ty` is anonymous.
    ///
    /// Also returns `true` if the Rust type has a lifetime parameter.
    fn type_expr(&mut self, ty: &'d Type, context: &str) -> Result<(TokenStream, bool), String> {
        if let [choice] = &ty.choices[..] {
            return self.type1_expr(choice, context);
        }

        let ident = make_ident(context)?;
        let has_lifetime = self.define_type(&ident, ty)?;
        Ok((
            if has_lifetime {
                quote!(#ident<'a>)
            } else {
                quote!(#ident)
            },
            has_lifetime,
        ))
    }

    /// Same as [`Generator::type_expr`], but for a single type choice.
    fn type1_expr(&mut self, ty: &'d Type1, context: &str) -> Result<(TokenStream, bool), String> {
        match &ty.operator {
            Some((Operator::Control(control), Type2::Value(Value::Integer(size))))
                if control == "size"
                    && matches!(&ty.target, Type2::Name { name, .. } if name == "bytes" || name == "bstr") =>
            {
                let Ok(size) = usize::try_from(*size) else {
                    return Err(format!("invalid byte string size: {size}"));
                };
                let size = proc_macro2::Literal::usize_unsuffixed(size);
                return Ok((quote!([u8; #size]), false));
            }
            Some((Operator::InclusiveRange | Operator::ExclusiveRange, end)) => {
                return match (&ty.target, end) {
                    (Type2::Value(Value::Integer(start)), Type2::Value(Value::Integer(end)))
                        if *start >= 0 && *end >= 0 =>
                    {
                        Ok((quote!(u64), false))
                    }
                    (Type2::Value(Value::Integer(_)), Type2::Value(Value::Integer(_))) => {
                        Ok((quote!(i64), false))
                    }
                    (Type2::Value(Value::Float(_)), Type2::Value(Value::Float(_))) => {
                        Ok((quote!(f64), false))
                    }
                    _ => Err(String::from(
                        "ranges whose bounds aren't both integer or float literals aren't \
                        supported",
                    )),
                };
            }
            // Other control operators only restrict the values of the target type.
            Some((Operator::Control(_), _)) | None => {}
        }

        match &ty.target {
            Type2::Name { name, .. } => Ok(match name.as_str() {
                "uint" => (quote!(u64), false),
                "nint" | "int" | "integer" => (quote!(i64), false),
                "bstr" | "bytes" => (quote!(&'a [u8]), true),
                "tstr" | "text" => (quote!(&'a str), true),
                "bool" | "true" | "false" => (quote!(bool), false),
                "float16" | "float32" => (quote!(f32), false),
                "float64" | "float16-32" | "float32-64" | "float" => (quote!(f64), false),
                "nil" | "null" => (quote!(()), false),
                name if self.rules.contains_key(name) => {
                    let ident = make_ident(&pascal_case(name))?;
                    if self.rule_type(name)? {
                        (quote!(#ident<'a>), true)
                    } else {
                        (quote!(#ident), false)
                    }
                }
                name => return Err(format!("type `{name}` isn't supported")),
            }),
            Type2::Parenthesized(ty) => self.type_expr(ty, context),
            Type2::Array(group) | Type2::Map(group) => {
                let is_map = matches!(ty.target, Type2::Map(_));

                if !is_collection(group, is_map) {
                    let ident = make_ident(context)?;
                    let has_lifetime = self.define_struct(&ident, group, is_map)?;
                    return Ok((
                        if has_lifetime {
                            quote!(#ident<'a>)
                        } else {
                            quote!(#ident)
                        },
                        has_lifetime,
                    ));
                }

                let Some(GroupEntry {
                    kind: GroupEntryKind::Member { key, ty: value },
                    ..
                }) = group
                    .choices
                    .first()
                    .and_then(|choice| choice.entries.first())
                else {
                    // Guaranteed by `is_collection`.
                    unreachable!()
                };

                if let Some(MemberKey::Type { ty: key, .. }) = key
                    && is_map
                {
                    let (key_type, key_has_lifetime) =
                        self.type1_expr(key, &format!("{context}Key"))?;
                    let (value_type, value_has_lifetime) =
                        self.type_expr(value, &format!("{context}Value"))?;
                    // TODO: make this borrow from the input instead of allocating
                    Ok((
                        quote!(alloc::collections::BTreeMap<#key_type, #value_type>),
                        key_has_lifetime || value_has_lifetime,
                    ))
                } else {
                    let (item_type, has_lifetime) =
                        self.type_expr(value, &format!("{context}Item"))?;
                    // TODO: make this borrow from the input instead of allocating
                    Ok((quote!(alloc::vec::Vec<#item_type>), has_lifetime))
                }
            }
            Type2::Value(_) => Err(String::from("literal values as types aren't supported")),
            Type2::Unwrap { .. } => Err(String::from("unwrapping (`~`) isn't supported")),
            Type2::ChoiceFromGroup(_) | Type2::ChoiceFromGroupName { .. } => {
                Err(String::from("choices from groups (`&`) aren't supported"))
            }
            Type2::Tag { .. } | Type2::MajorType { .. } | Type2::Any => {
                Err(String::from("tags and major types (`#`) aren't supported"))
            }
        }
    }

    /// Adds a Rust item defining a type named `ident`.
    fn register_type(&mut self, ident: &Ident, item: TokenStream) -> Result<(), String> {
        if !self.type_names.insert(ident.to_string()) {
            return Err(format!("multiple types named `{ident}`"));
        }
        self.items.push(quote! {
            #[derive(Debug, Clone, PartialEq)]
            #item
        });
        Ok(())
    }
}

/// Returns `true` if the given array or map consists in a single entry that can be repeated,
/// such as `[ * uint ]` or `{ * tstr => uint }`, in which case it is represented as a `Vec` or
/// a `BTreeMap`.
fn is_collection(group: &Group, is_map: bool) -> bool {
    let [GroupChoice { entries }] = &group.choices[..] else {
        return false;
    };
    let [
        GroupEntry {
            occurrence: Some(occurrence),
            kind: GroupEntryKind::Member { key, .. },
        },
    ] = &entries[..]
    else {
        return false;
    };
    if occurrence.max == Some(1) {
        return false;
    }
    if is_map {
        matches!(key, Some(MemberKey::Type { .. }))
    } else {
        matches!(key, None | Some(MemberKey::Bareword(_)))
    }
}

/// Builds a Rust identifier, using a raw identifier if `name` is a keyword.
fn make_ident(name: &str) -> Result<Ident, String> {
    if let Ok(ident) = syn::parse_str::<Ident>(name) {
        return Ok(ident);
    }
    syn::parse_str::<Ident>(&format!("r#{name}"))
        .map_err(|_| format!("`{name}` can't be converted to a Rust identifier"))
}

/// Converts a CDDL name, such as `foo-bar`, to snake_case, such as `foo_bar`.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut previous_is_lower_or_digit = false;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            previous_is_lower_or_digit = false;
            continue;
        }
        if c.is_ascii_uppercase() && previous_is_lower_or_digit {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
        previous_is_lower_or_digit = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    if out.ends_with('_') {
        out.pop();
    }
    out
}

/// Converts a CDDL name, such as `foo-bar`, to PascalCase, such as `FooBar`.
fn pascal_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut capitalize = true;
    for c in name.chars() {
        if !c.is_ascii_alphanumeric() {
            capitalize = true;
            continue;
        }
        out.push(if capitalize {
            c.to_ascii_uppercase()
        } else {
            c
        });
        capitalize = false;
    }
    out
}
