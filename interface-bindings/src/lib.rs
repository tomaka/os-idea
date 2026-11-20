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
/// - A `decode(data)` function on every generated type, that decodes a value of that type from
///   its CBOR encoding, and a `DecodeError` type returned when decoding fails. Indefinite-length
///   CBOR items and tags aren't supported. A `decode_partial(data)` function is also generated,
///   that decodes a value from the start of `data` and returns it alongside the number of bytes
///   consumed.
/// - An `encode_into(self, out)` function on every generated type, that encodes the value to
///   CBOR by calling `out` one or more times. The concatenation of all the slices passed to `out`
///   forms the encoding.
/// - A `HASH: [u8; 32]` constant containing the hash of the structure of the interface, as
///   returned by `interface_parser::InterfaceDefinition::hash`.
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

    let hash = definition.hash();

    quote! {
        pub mod #module_name {
            #![allow(dead_code)]
            // Generated code that decodes optional values uses `Ok(foo?)`.
            #![allow(clippy::needless_question_mark)]

            extern crate alloc;

            // Makes Cargo rebuild the invoking crate whenever the interface definition changes.
            const _: &[u8] = include_bytes!(#path_str);

            /// Hash of the structure of the interface definition, as defined by the
            /// interface definition hashing rules.
            pub const HASH: [u8; 32] = [#(#hash),*];

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
        // This name is used by the decoding code generated below.
        type_names: BTreeSet::from([String::from("DecodeError")]),
        items: Vec::new(),
    };

    for rule in &definition.rules {
        // The root rule and its extensions are covered by `definition.messages`.
        if rule.name == root_name {
            continue;
        }
        // FIXME: support extending rules with `/=` and `//=`
        if rule.assignment != Assignment::Define {
            return Err(format!(
                "extending rule `{}` with `/=` or `//=` isn't supported",
                rule.name
            ));
        }
        // FIXME: support generic rules
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
                let response =
                    generator.type_expr(response, &format!("{request_ident}Response"))?;
                let response_type = response.ty;
                (
                    if response.has_lifetime {
                        quote!(<'a>)
                    } else {
                        quote!()
                    },
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
        /// Error that can happen when decoding a value from CBOR.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum DecodeError {
            /// The data ends in the middle of an item.
            UnexpectedEnd,
            /// The data isn't valid CBOR.
            Malformed,
            /// The data contains an indefinite-length item, which isn't supported.
            IndefiniteLength,
            /// An item doesn't have the expected CBOR type.
            UnexpectedType,
            /// An item has the expected CBOR type, but its value isn't allowed.
            InvalidValue,
            /// A text string isn't valid UTF-8.
            InvalidUtf8,
            /// An array or a map doesn't have the expected number of items.
            WrongItemCount,
            /// A map contains an unknown key.
            UnknownKey,
            /// A map doesn't contain a required key.
            MissingKey,
            /// A map contains the same key multiple times.
            DuplicateKey,
            /// The item doesn't match any of the possible choices of a type.
            NoMatchingChoice,
            /// The data continues after the end of the value.
            TrailingData,
        }

        impl core::fmt::Display for DecodeError {
            fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                formatter.write_str(match self {
                    DecodeError::UnexpectedEnd => "unexpected end of data",
                    DecodeError::Malformed => "malformed CBOR",
                    DecodeError::IndefiniteLength => "indefinite-length items aren't supported",
                    DecodeError::UnexpectedType => "unexpected CBOR type",
                    DecodeError::InvalidValue => "invalid value",
                    DecodeError::InvalidUtf8 => "text string isn't valid UTF-8",
                    DecodeError::WrongItemCount => "wrong number of items",
                    DecodeError::UnknownKey => "unknown map key",
                    DecodeError::MissingKey => "missing map key",
                    DecodeError::DuplicateKey => "duplicate map key",
                    DecodeError::NoMatchingChoice => "no matching type choice",
                    DecodeError::TrailingData => "trailing data",
                })
            }
        }

        impl core::error::Error for DecodeError {}

        /// Implementation details of the decoding and encoding.
        mod __impl {
            use super::DecodeError;

            /// Reads CBOR items one after the other.
            pub(super) struct Decoder<'a> {
                /// Data that hasn't been decoded yet.
                pub(super) remaining: &'a [u8],
            }

            impl<'a> Decoder<'a> {
                pub(super) fn take(&mut self, len: usize) -> ::core::result::Result<&'a [u8], DecodeError> {
                    let Some((taken, rest)) = self.remaining.split_at_checked(len) else {
                        return Err(DecodeError::UnexpectedEnd);
                    };
                    self.remaining = rest;
                    Ok(taken)
                }

                pub(super) fn take_array<const N: usize>(
                    &mut self,
                ) -> ::core::result::Result<[u8; N], DecodeError> {
                    let taken = self.take(N)?;
                    // `take` always returns exactly `N` bytes.
                    Ok(<[u8; N]>::try_from(taken).unwrap_or_else(|_| unreachable!()))
                }

                /// Reads the head of the next item, and returns its major type, its additional
                /// information, and its argument.
                pub(super) fn head(&mut self) -> ::core::result::Result<(u8, u8, u64), DecodeError> {
                    // FIXME: tags (major type 6) are never skipped, meaning that tagged items always
                    // fail to decode with `UnexpectedType`
                    let [initial] = self.take_array::<1>()?;
                    let major = initial >> 5;
                    let info = initial & 0x1f;
                    let argument = match info {
                        0..=23 => u64::from(info),
                        24 => u64::from(u8::from_be_bytes(self.take_array()?)),
                        25 => u64::from(u16::from_be_bytes(self.take_array()?)),
                        26 => u64::from(u32::from_be_bytes(self.take_array()?)),
                        27 => u64::from_be_bytes(self.take_array()?),
                        // FIXME: support indefinite-length arrays and maps, and maybe also byte and
                        // text strings (which, however, can't be borrowed from the input)
                        31 if (2..=5).contains(&major) => return Err(DecodeError::IndefiniteLength),
                        _ => return Err(DecodeError::Malformed),
                    };
                    Ok((major, info, argument))
                }

                /// Decodes a value with `decode`. If it fails, rewinds the decoder to its state
                /// before the call and returns `None`.
                pub(super) fn attempt<T>(
                    &mut self,
                    decode: impl FnOnce(&mut Self) -> ::core::result::Result<T, DecodeError>,
                ) -> ::core::option::Option<T> {
                    let snapshot = self.remaining;
                    let outcome = decode(self);
                    if outcome.is_err() {
                        self.remaining = snapshot;
                    }
                    outcome.ok()
                }

                pub(super) fn uint(&mut self) -> ::core::result::Result<u64, DecodeError> {
                    match self.head()? {
                        (0, _, argument) => Ok(argument),
                        _ => Err(DecodeError::UnexpectedType),
                    }
                }

                pub(super) fn int(&mut self) -> ::core::result::Result<i64, DecodeError> {
                    match self.head()? {
                        (0, _, argument) => {
                            i64::try_from(argument).map_err(|_| DecodeError::InvalidValue)
                        }
                        (1, _, argument) => i64::try_from(argument)
                            .map(|argument| -1 - argument)
                            .map_err(|_| DecodeError::InvalidValue),
                        _ => Err(DecodeError::UnexpectedType),
                    }
                }

                pub(super) fn nint(&mut self) -> ::core::result::Result<i64, DecodeError> {
                    let value = self.int()?;
                    if value >= 0 {
                        return Err(DecodeError::UnexpectedType);
                    }
                    Ok(value)
                }

                pub(super) fn bytes(&mut self) -> ::core::result::Result<&'a [u8], DecodeError> {
                    let (2, _, len) = self.head()? else {
                        return Err(DecodeError::UnexpectedType);
                    };
                    self.take(usize::try_from(len).map_err(|_| DecodeError::UnexpectedEnd)?)
                }

                pub(super) fn text(&mut self) -> ::core::result::Result<&'a str, DecodeError> {
                    let (3, _, len) = self.head()? else {
                        return Err(DecodeError::UnexpectedType);
                    };
                    let bytes =
                        self.take(usize::try_from(len).map_err(|_| DecodeError::UnexpectedEnd)?)?;
                    core::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8)
                }

                /// Reads the head of an array and returns its number of items.
                pub(super) fn array(&mut self) -> ::core::result::Result<u64, DecodeError> {
                    match self.head()? {
                        (4, _, len) => Ok(len),
                        _ => Err(DecodeError::UnexpectedType),
                    }
                }

                /// Reads the head of a map and returns its number of entries.
                pub(super) fn map(&mut self) -> ::core::result::Result<u64, DecodeError> {
                    match self.head()? {
                        (5, _, len) => Ok(len),
                        _ => Err(DecodeError::UnexpectedType),
                    }
                }

                pub(super) fn bool(&mut self) -> ::core::result::Result<bool, DecodeError> {
                    match self.head()? {
                        (7, 20, _) => Ok(false),
                        (7, 21, _) => Ok(true),
                        _ => Err(DecodeError::UnexpectedType),
                    }
                }

                pub(super) fn null(&mut self) -> ::core::result::Result<(), DecodeError> {
                    match self.head()? {
                        (7, 22, _) => Ok(()),
                        _ => Err(DecodeError::UnexpectedType),
                    }
                }

                /// Reads a half-precision or single-precision float.
                pub(super) fn float32(&mut self) -> ::core::result::Result<f32, DecodeError> {
                    match self.head()? {
                        (7, 25, argument) => {
                            // The argument of a 25 additional information is always 16 bits.
                            let half = u16::try_from(argument).unwrap_or_else(|_| unreachable!());
                            let exponent = u32::from((half >> 10) & 0x1f);
                            let mantissa = u32::from(half & 0x3ff);
                            let magnitude = match exponent {
                                0 => f32::from(half & 0x3ff) * (1.0 / 16_777_216.0),
                                0x1f => f32::from_bits(0x7f80_0000 | (mantissa << 13)),
                                _ => f32::from_bits(((exponent + 112) << 23) | (mantissa << 13)),
                            };
                            Ok(f32::from_bits(
                                magnitude.to_bits() | (u32::from(half >> 15) << 31),
                            ))
                        }
                        (7, 26, argument) => {
                            // The argument of a 26 additional information is always 32 bits.
                            let bits = u32::try_from(argument).unwrap_or_else(|_| unreachable!());
                            Ok(f32::from_bits(bits))
                        }
                        _ => Err(DecodeError::UnexpectedType),
                    }
                }

                /// Reads a half-precision, single-precision, or double-precision float.
                pub(super) fn float(&mut self) -> ::core::result::Result<f64, DecodeError> {
                    let snapshot = self.remaining;
                    if let (7, 27, argument) = self.head()? {
                        return Ok(f64::from_bits(argument));
                    }
                    self.remaining = snapshot;
                    Ok(f64::from(self.float32()?))
                }
            }

            /// Writes CBOR items one after the other.
            pub(super) struct Encoder<F> {
                /// Function called with the encoded data.
                pub(super) out: F,
            }

            impl<F: FnMut(&[u8])> Encoder<F> {
                /// Writes the head of an item, using the shortest possible encoding of `argument`.
                pub(super) fn head(&mut self, major: u8, argument: u64) {
                    let major = major << 5;
                    if let Ok(argument) = u8::try_from(argument) {
                        if argument < 24 {
                            (self.out)(&[major | argument]);
                        } else {
                            (self.out)(&[major | 24, argument]);
                        }
                    } else if let Ok(argument) = u16::try_from(argument) {
                        let [b0, b1] = argument.to_be_bytes();
                        (self.out)(&[major | 25, b0, b1]);
                    } else if let Ok(argument) = u32::try_from(argument) {
                        let [b0, b1, b2, b3] = argument.to_be_bytes();
                        (self.out)(&[major | 26, b0, b1, b2, b3]);
                    } else {
                        let [b0, b1, b2, b3, b4, b5, b6, b7] = argument.to_be_bytes();
                        (self.out)(&[major | 27, b0, b1, b2, b3, b4, b5, b6, b7]);
                    }
                }

                /// Writes the head of an item whose argument is a length.
                pub(super) fn length_head(&mut self, major: u8, len: usize) {
                    // `usize` is never larger than 64 bits on supported platforms.
                    self.head(major, u64::try_from(len).unwrap_or_else(|_| unreachable!()));
                }

                pub(super) fn uint(&mut self, value: u64) {
                    self.head(0, value);
                }

                pub(super) fn int(&mut self, value: i64) {
                    if let Ok(value) = u64::try_from(value) {
                        self.head(0, value);
                    } else {
                        // `value` is negative, so `-1 - value` is always positive.
                        self.head(1, u64::try_from(-1 - value).unwrap_or_else(|_| unreachable!()));
                    }
                }

                pub(super) fn bytes(&mut self, value: &[u8]) {
                    self.length_head(2, value.len());
                    (self.out)(value);
                }

                pub(super) fn text(&mut self, value: &str) {
                    self.length_head(3, value.len());
                    (self.out)(value.as_bytes());
                }

                /// Writes the head of an array of `len` items.
                pub(super) fn array(&mut self, len: usize) {
                    self.length_head(4, len);
                }

                /// Writes the head of a map of `len` entries.
                pub(super) fn map(&mut self, len: usize) {
                    self.length_head(5, len);
                }

                pub(super) fn bool(&mut self, value: bool) {
                    (self.out)(&[if value { 0xf5 } else { 0xf4 }]);
                }

                pub(super) fn null(&mut self) {
                    (self.out)(&[0xf6]);
                }

                /// Writes a single-precision float.
                pub(super) fn float32(&mut self, value: f32) {
                    let [b0, b1, b2, b3] = value.to_bits().to_be_bytes();
                    (self.out)(&[0xfa, b0, b1, b2, b3]);
                }

                /// Writes a double-precision float.
                pub(super) fn float(&mut self, value: f64) {
                    let [b0, b1, b2, b3, b4, b5, b6, b7] = value.to_bits().to_be_bytes();
                    (self.out)(&[0xfb, b0, b1, b2, b3, b4, b5, b6, b7]);
                }
            }
        }

        #(#items)*
        #(#emit_functions)*
    })
}

/// Rust type corresponding to a CDDL type.
struct TypeExpr {
    /// The Rust type.
    ty: TokenStream,
    /// `true` if [`TypeExpr::ty`] has a lifetime parameter.
    has_lifetime: bool,
    /// Expression whose type is [`TypeExpr::ty`], that decodes a value from a variable named
    /// `decoder` of type `&mut __impl::Decoder`. The expression can use the `?` operator and `return`
    /// statements on `Result<_, DecodeError>`.
    decode: TokenStream,
    /// Statements that encode a variable named `value` whose type is [`TypeExpr::ty`] to a
    /// variable named `encoder` of type `&mut __impl::Encoder`.
    encode: TokenStream,
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

        // FIXME: support recursive rules, for example by boxing
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

            let inner = self.type1_expr(choice, &format!("{ident}Inner"))?;
            let (inner_type, inner_decode, inner_encode) = (inner.ty, inner.decode, inner.encode);
            let generics = if inner.has_lifetime {
                quote!(<'a>)
            } else {
                quote!()
            };
            self.register_type(
                ident,
                inner.has_lifetime,
                quote!(pub struct #ident #generics (pub #inner_type);),
                quote!(Ok(Self(#inner_decode))),
                quote! {
                    let Self(value) = self;
                    #inner_encode
                },
            )?;
            return Ok(inner.has_lifetime);
        }

        let mut variant_names = BTreeSet::new();
        let mut variants = Vec::with_capacity(ty.choices.len());
        let mut variant_decodes = Vec::with_capacity(ty.choices.len());
        let mut variant_encodes = Vec::with_capacity(ty.choices.len());
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
            let payload = self.type1_expr(choice, &format!("{ident}{variant_name}"))?;
            if !variant_names.insert(variant_name) {
                return Err(format!(
                    "type `{ident}` has multiple variants named `{variant_ident}`"
                ));
            }
            has_lifetime |= payload.has_lifetime;
            let (payload_type, payload_decode, payload_encode) =
                (payload.ty, payload.decode, payload.encode);
            variant_encodes.push(quote!(Self::#variant_ident(value) => { #payload_encode }));
            variants.push(quote!(#variant_ident(#payload_type)));
            // Choices are tried in the order in which they're declared.
            variant_decodes.push(quote! {
                if let Some(value) =
                    decoder.attempt(|decoder| Ok(Self::#variant_ident(#payload_decode)))
                {
                    return Ok(value);
                }
            });
        }

        let generics = if has_lifetime { quote!(<'a>) } else { quote!() };
        self.register_type(
            ident,
            has_lifetime,
            quote!(pub enum #ident #generics { #(#variants),* }),
            quote! {
                #(#variant_decodes)*
                Err(DecodeError::NoMatchingChoice)
            },
            quote! {
                match self {
                    #(#variant_encodes)*
                }
            },
        )?;
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
        // FIXME: support group choices (`//`), for example by generating an enum
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
        // FIXME: support maps whose keys are integers or other non-name values
        if is_map && !named {
            return Err(format!(
                "map `{ident}` must only contain members whose key is a name"
            ));
        }

        let mut field_names = BTreeSet::new();
        let mut fields = Vec::with_capacity(members.len());
        let mut field_idents = Vec::with_capacity(members.len());
        // Local variables in which the decoded fields are stored, in the order of the fields.
        let mut locals = Vec::with_capacity(members.len());
        // Statements decoding the items of the array, or arms of the `match` on the map keys.
        let mut decode_steps = Vec::with_capacity(members.len());
        // For maps, statements executed after all the entries have been decoded.
        let mut map_finalizers = Vec::with_capacity(members.len());
        // For maps, statements declaring the local variables.
        let mut map_initializers = Vec::with_capacity(members.len());
        // Expressions whose sum is the number of items in the array or entries in the map.
        let mut encode_counts = Vec::with_capacity(members.len());
        // Statements encoding each field.
        let mut encode_steps = Vec::with_capacity(members.len());
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
            let field = self.type_expr(ty, &context)?;
            has_lifetime |= field.has_lifetime;
            let (mut field_type, field_decode, field_encode) =
                (field.ty, field.decode, field.encode);
            let local = quote::format_ident!("field{index}");

            // In maps, each value is preceded with its key.
            let encode_key = if is_map {
                let Some(MemberKey::Bareword(key) | MemberKey::Value(Value::Text(key))) = key
                else {
                    // Verified above when computing `named`.
                    unreachable!()
                };
                quote!(encoder.text(#key);)
            } else {
                quote!()
            };
            match member.occurrence {
                None
                | Some(Occurrence {
                    min: 1,
                    max: Some(1),
                }) => {
                    encode_counts.push(quote!(1));
                    encode_steps.push(quote! {
                        let value = #local;
                        #encode_key
                        #field_encode
                    });
                }
                Some(Occurrence {
                    min: 0,
                    max: Some(1),
                }) => {
                    encode_counts.push(quote!(usize::from(#local.is_some())));
                    encode_steps.push(quote! {
                        if let Some(value) = #local {
                            #encode_key
                            #field_encode
                        }
                    });
                }
                Some(_) => {
                    encode_counts.push(quote!(#local.len()));
                    encode_steps.push(quote! {
                        for value in #local {
                            #encode_key
                            #field_encode
                        }
                    });
                }
            }

            // Checks whether the number of items in `#local` matches the occurrence indicator.
            let count_checks = match member.occurrence {
                Some(Occurrence { min, max }) => {
                    let min_check = (min != 0).then(|| {
                        quote! {
                            if u64::try_from(#local.len()).unwrap_or(u64::MAX) < #min {
                                return Err(DecodeError::WrongItemCount);
                            }
                        }
                    });
                    let max_check = max.map(|max| {
                        quote! {
                            if u64::try_from(#local.len()).unwrap_or(u64::MAX) > #max {
                                return Err(DecodeError::WrongItemCount);
                            }
                        }
                    });
                    quote!(#min_check #max_check)
                }
                None => quote!(),
            };

            match (member.occurrence, is_map) {
                (
                    None
                    | Some(Occurrence {
                        min: 1,
                        max: Some(1),
                    }),
                    false,
                ) => {
                    decode_steps.push(quote! {
                        if remaining == 0 {
                            return Err(DecodeError::WrongItemCount);
                        }
                        remaining -= 1;
                        let #local = #field_decode;
                    });
                }
                (
                    Some(Occurrence {
                        min: 0,
                        max: Some(1),
                    }),
                    false,
                ) => {
                    field_type = quote!(::core::option::Option<#field_type>);
                    decode_steps.push(quote! {
                        let #local = if remaining == 0 {
                            None
                        } else {
                            let value = decoder.attempt(|decoder| Ok(#field_decode));
                            if value.is_some() {
                                remaining -= 1;
                            }
                            value
                        };
                    });
                }
                (Some(Occurrence { max, .. }), false) => {
                    // TODO: make this borrow from the input instead of allocating
                    field_type = quote!(alloc::vec::Vec<#field_type>);
                    let max_condition = max.map(
                        |max| quote!(&& u64::try_from(#local.len()).unwrap_or(u64::MAX) < #max),
                    );
                    decode_steps.push(quote! {
                        let mut #local = alloc::vec::Vec::new();
                        while remaining != 0 #max_condition {
                            let Some(value) = decoder.attempt(|decoder| Ok(#field_decode)) else {
                                break;
                            };
                            remaining -= 1;
                            #local.push(value);
                        }
                        #count_checks
                    });
                }
                (occurrence, true) => {
                    let Some(MemberKey::Bareword(key) | MemberKey::Value(Value::Text(key))) = key
                    else {
                        // Verified above when computing `named`.
                        unreachable!()
                    };
                    match occurrence {
                        None
                        | Some(Occurrence {
                            min: 1,
                            max: Some(1),
                        }) => {
                            map_initializers.push(quote!(let mut #local = None;));
                            decode_steps.push(quote! {
                                #key => {
                                    if #local.is_some() {
                                        return Err(DecodeError::DuplicateKey);
                                    }
                                    #local = Some(#field_decode);
                                }
                            });
                            map_finalizers.push(quote! {
                                let Some(#local) = #local else {
                                    return Err(DecodeError::MissingKey);
                                };
                            });
                        }
                        Some(Occurrence {
                            min: 0,
                            max: Some(1),
                        }) => {
                            field_type = quote!(::core::option::Option<#field_type>);
                            map_initializers.push(quote!(let mut #local = None;));
                            decode_steps.push(quote! {
                                #key => {
                                    if #local.is_some() {
                                        return Err(DecodeError::DuplicateKey);
                                    }
                                    #local = Some(#field_decode);
                                }
                            });
                        }
                        Some(_) => {
                            // TODO: make this borrow from the input instead of allocating
                            field_type = quote!(alloc::vec::Vec<#field_type>);
                            map_initializers.push(quote!(let mut #local = alloc::vec::Vec::new();));
                            // FIXME: a CBOR map can't contain the same key multiple times, so
                            // this accepts duplicate keys that it should reject
                            decode_steps.push(quote!(#key => #local.push(#field_decode),));
                            map_finalizers.push(count_checks);
                        }
                    }
                }
            }

            fields.push(match &field_ident {
                Some(field_ident) => quote!(pub #field_ident: #field_type),
                None => quote!(pub #field_type),
            });
            field_idents.push(field_ident);
            locals.push(local);
        }

        let construct = if named {
            let field_idents = field_idents.into_iter().flatten();
            quote!(Self { #(#field_idents: #locals),* })
        } else {
            quote!(Self(#(#locals),*))
        };

        let encode_count = if encode_counts.is_empty() {
            quote!(0)
        } else {
            quote!(#(#encode_counts)+*)
        };
        let encode_head = if is_map {
            quote!(encoder.map(#encode_count);)
        } else {
            quote!(encoder.array(#encode_count);)
        };
        let encode = quote! {
            let #construct = self;
            #encode_head
            #(#encode_steps)*
        };

        let decode = if is_map {
            quote! {
                let count = decoder.map()?;
                #(#map_initializers)*
                for _ in 0..count {
                    match decoder.text()? {
                        #(#decode_steps)*
                        _ => return Err(DecodeError::UnknownKey),
                    }
                }
                #(#map_finalizers)*
                Ok(#construct)
            }
        } else {
            let mutability = if members.is_empty() {
                quote!()
            } else {
                quote!(mut)
            };
            quote! {
                let #mutability remaining = decoder.array()?;
                #(#decode_steps)*
                if remaining != 0 {
                    return Err(DecodeError::WrongItemCount);
                }
                Ok(#construct)
            }
        };

        let generics = if has_lifetime { quote!(<'a>) } else { quote!() };
        let item = if named {
            quote!(pub struct #ident #generics { #(#fields),* })
        } else {
            quote!(pub struct #ident #generics (#(#fields),*);)
        };
        self.register_type(ident, has_lifetime, item, decode, encode)?;
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
                    // FIXME: support occurrence indicators on groups
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
                    // FIXME: support group choices and occurrence indicators on inline groups
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
    fn type_expr(&mut self, ty: &'d Type, context: &str) -> Result<TypeExpr, String> {
        if let [choice] = &ty.choices[..] {
            return self.type1_expr(choice, context);
        }

        let ident = make_ident(context)?;
        let has_lifetime = self.define_type(&ident, ty)?;
        Ok(TypeExpr {
            ty: if has_lifetime {
                quote!(#ident<'a>)
            } else {
                quote!(#ident)
            },
            has_lifetime,
            decode: quote!(#ident::decode_from(decoder)?),
            encode: quote!(#ident::encode_to(value, encoder);),
        })
    }

    /// Same as [`Generator::type_expr`], but for a single type choice.
    fn type1_expr(&mut self, ty: &'d Type1, context: &str) -> Result<TypeExpr, String> {
        match &ty.operator {
            Some((Operator::Control(control), Type2::Value(Value::Integer(size))))
                if control == "size"
                    && matches!(&ty.target, Type2::Name { name, .. } if name == "bytes" || name == "bstr") =>
            {
                let Ok(size) = usize::try_from(*size) else {
                    return Err(format!("invalid byte string size: {size}"));
                };
                let size = proc_macro2::Literal::usize_unsuffixed(size);
                return Ok(TypeExpr {
                    ty: quote!([u8; #size]),
                    has_lifetime: false,
                    decode: quote! {
                        <[u8; #size]>::try_from(decoder.bytes()?)
                            .map_err(|_| DecodeError::InvalidValue)?
                    },
                    encode: quote!(encoder.bytes(&value);),
                });
            }
            Some((operator @ (Operator::InclusiveRange | Operator::ExclusiveRange), end)) => {
                let range = if matches!(operator, Operator::InclusiveRange) {
                    quote!(..=)
                } else {
                    quote!(..)
                };
                let (ty, decode, encode, start, end) = match (&ty.target, end) {
                    (Type2::Value(Value::Integer(start)), Type2::Value(Value::Integer(end)))
                        if *start >= 0 && *end >= 0 =>
                    {
                        let (Ok(start), Ok(end)) = (u64::try_from(*start), u64::try_from(*end))
                        else {
                            return Err(format!("range bounds {start} and {end} are too large"));
                        };
                        (
                            quote!(u64),
                            quote!(decoder.uint()?),
                            quote!(encoder.uint(value);),
                            proc_macro2::Literal::u64_suffixed(start),
                            proc_macro2::Literal::u64_suffixed(end),
                        )
                    }
                    (Type2::Value(Value::Integer(start)), Type2::Value(Value::Integer(end))) => {
                        let (Ok(start), Ok(end)) = (i64::try_from(*start), i64::try_from(*end))
                        else {
                            return Err(format!("range bounds {start} and {end} are too large"));
                        };
                        (
                            quote!(i64),
                            quote!(decoder.int()?),
                            quote!(encoder.int(value);),
                            proc_macro2::Literal::i64_suffixed(start),
                            proc_macro2::Literal::i64_suffixed(end),
                        )
                    }
                    (Type2::Value(Value::Float(start)), Type2::Value(Value::Float(end)))
                        if start.is_finite() && end.is_finite() =>
                    {
                        (
                            quote!(f64),
                            quote!(decoder.float()?),
                            quote!(encoder.float(value);),
                            proc_macro2::Literal::f64_suffixed(*start),
                            proc_macro2::Literal::f64_suffixed(*end),
                        )
                    }
                    // FIXME: support ranges whose bounds are references to rules or non-finite
                    // floats
                    _ => {
                        return Err(String::from(
                            "ranges whose bounds aren't both integer or finite float literals \
                            aren't supported",
                        ));
                    }
                };
                return Ok(TypeExpr {
                    ty,
                    has_lifetime: false,
                    decode: quote! {{
                        let value = #decode;
                        if !(#start #range #end).contains(&value) {
                            return Err(DecodeError::InvalidValue);
                        }
                        value
                    }},
                    encode,
                });
            }
            // Other control operators only restrict the values of the target type.
            // FIXME: these restrictions aren't verified when decoding
            Some((Operator::Control(_), _)) | None => {}
        }

        match &ty.target {
            Type2::Name { name, .. } => {
                let (ty, has_lifetime, decode, encode) = match name.as_str() {
                    "uint" => (
                        quote!(u64),
                        false,
                        quote!(decoder.uint()?),
                        quote!(encoder.uint(value);),
                    ),
                    "nint" => (
                        quote!(i64),
                        false,
                        quote!(decoder.nint()?),
                        quote!(encoder.int(value);),
                    ),
                    "int" | "integer" => (
                        quote!(i64),
                        false,
                        quote!(decoder.int()?),
                        quote!(encoder.int(value);),
                    ),
                    "bstr" | "bytes" => (
                        quote!(&'a [u8]),
                        true,
                        quote!(decoder.bytes()?),
                        quote!(encoder.bytes(value);),
                    ),
                    "tstr" | "text" => (
                        quote!(&'a str),
                        true,
                        quote!(decoder.text()?),
                        quote!(encoder.text(value);),
                    ),
                    "bool" => (
                        quote!(bool),
                        false,
                        quote!(decoder.bool()?),
                        quote!(encoder.bool(value);),
                    ),
                    "true" | "false" => {
                        let expected = name == "true";
                        (
                            quote!(bool),
                            false,
                            quote! {{
                                if decoder.bool()? != #expected {
                                    return Err(DecodeError::InvalidValue);
                                }
                                #expected
                            }},
                            quote!(encoder.bool(value);),
                        )
                    }
                    "float16" | "float32" => (
                        quote!(f32),
                        false,
                        quote!(decoder.float32()?),
                        quote!(encoder.float32(value);),
                    ),
                    "float64" | "float16-32" | "float32-64" | "float" => (
                        quote!(f64),
                        false,
                        quote!(decoder.float()?),
                        quote!(encoder.float(value);),
                    ),
                    "nil" | "null" => (
                        quote!(()),
                        false,
                        quote!(decoder.null()?),
                        quote! {
                            let () = value;
                            encoder.null();
                        },
                    ),
                    name if self.rules.contains_key(name) => {
                        let ident = make_ident(&pascal_case(name))?;
                        let decode = quote!(#ident::decode_from(decoder)?);
                        let encode = quote!(#ident::encode_to(value, encoder););
                        if self.rule_type(name)? {
                            (quote!(#ident<'a>), true, decode, encode)
                        } else {
                            (quote!(#ident), false, decode, encode)
                        }
                    }
                    // FIXME: support the other prelude types, such as `any`, `undefined` or
                    // `bigint`
                    name => return Err(format!("type `{name}` isn't supported")),
                };
                Ok(TypeExpr {
                    ty,
                    has_lifetime,
                    decode,
                    encode,
                })
            }
            Type2::Parenthesized(ty) => self.type_expr(ty, context),
            Type2::Array(group) | Type2::Map(group) => {
                let is_map = matches!(ty.target, Type2::Map(_));

                if !is_collection(group, is_map) {
                    let ident = make_ident(context)?;
                    let has_lifetime = self.define_struct(&ident, group, is_map)?;
                    return Ok(TypeExpr {
                        ty: if has_lifetime {
                            quote!(#ident<'a>)
                        } else {
                            quote!(#ident)
                        },
                        has_lifetime,
                        decode: quote!(#ident::decode_from(decoder)?),
                        encode: quote!(#ident::encode_to(value, encoder);),
                    });
                }

                let Some(GroupEntry {
                    kind: GroupEntryKind::Member { key, ty: value },
                    occurrence: Some(Occurrence { min, max }),
                }) = group
                    .choices
                    .first()
                    .and_then(|choice| choice.entries.first())
                else {
                    // Guaranteed by `is_collection`.
                    unreachable!()
                };

                let min_check = (*min != 0).then(|| {
                    quote! {
                        if count < #min {
                            return Err(DecodeError::WrongItemCount);
                        }
                    }
                });
                let max_check = max.map(|max| {
                    quote! {
                        if count > #max {
                            return Err(DecodeError::WrongItemCount);
                        }
                    }
                });

                if let Some(MemberKey::Type { ty: key, .. }) = key
                    && is_map
                {
                    let key = self.type1_expr(key, &format!("{context}Key"))?;
                    let value = self.type_expr(value, &format!("{context}Value"))?;
                    let (key_type, key_decode, key_encode) = (key.ty, key.decode, key.encode);
                    let (value_type, value_decode, value_encode) =
                        (value.ty, value.decode, value.encode);
                    // TODO: make this borrow from the input instead of allocating
                    Ok(TypeExpr {
                        ty: quote!(alloc::collections::BTreeMap<#key_type, #value_type>),
                        has_lifetime: key.has_lifetime || value.has_lifetime,
                        decode: quote! {{
                            let count = decoder.map()?;
                            #min_check
                            #max_check
                            let mut entries = alloc::collections::BTreeMap::new();
                            for _ in 0..count {
                                let key = #key_decode;
                                let value = #value_decode;
                                if entries.insert(key, value).is_some() {
                                    return Err(DecodeError::DuplicateKey);
                                }
                            }
                            entries
                        }},
                        encode: quote! {
                            encoder.map(value.len());
                            for (key, entry_value) in value {
                                {
                                    let value = key;
                                    #key_encode
                                }
                                {
                                    let value = entry_value;
                                    #value_encode
                                }
                            }
                        },
                    })
                } else {
                    let item = self.type_expr(value, &format!("{context}Item"))?;
                    let (item_type, item_decode, item_encode) = (item.ty, item.decode, item.encode);
                    // TODO: make this borrow from the input instead of allocating
                    Ok(TypeExpr {
                        ty: quote!(alloc::vec::Vec<#item_type>),
                        has_lifetime: item.has_lifetime,
                        decode: quote! {{
                            let count = decoder.array()?;
                            #min_check
                            #max_check
                            let mut items = alloc::vec::Vec::new();
                            for _ in 0..count {
                                items.push(#item_decode);
                            }
                            items
                        }},
                        encode: quote! {
                            encoder.array(value.len());
                            for value in value {
                                #item_encode
                            }
                        },
                    })
                }
            }
            // FIXME: support literal values as types
            Type2::Value(_) => Err(String::from("literal values as types aren't supported")),
            // FIXME: support unwrapping (`~`)
            Type2::Unwrap { .. } => Err(String::from("unwrapping (`~`) isn't supported")),
            // FIXME: support choices from groups (`&`)
            Type2::ChoiceFromGroup(_) | Type2::ChoiceFromGroupName { .. } => {
                Err(String::from("choices from groups (`&`) aren't supported"))
            }
            // FIXME: support tags, major types (`#`), and `#` alone (any type)
            Type2::Tag { .. } | Type2::MajorType { .. } | Type2::Any => {
                Err(String::from("tags and major types (`#`) aren't supported"))
            }
        }
    }

    /// Adds a Rust item defining a type named `ident`, alongside its decoding and encoding
    /// functions.
    ///
    /// `decode_body` is the body of a function that returns `Result<Self, DecodeError>`, and
    /// that decodes the value from a variable named `decoder` of type `&mut __impl::Decoder`.
    ///
    /// `encode_body` is the body of a function that encodes `self` to a variable named `encoder`
    /// of type `&mut __impl::Encoder`.
    fn register_type(
        &mut self,
        ident: &Ident,
        has_lifetime: bool,
        item: TokenStream,
        decode_body: TokenStream,
        encode_body: TokenStream,
    ) -> Result<(), String> {
        if !self.type_names.insert(ident.to_string()) {
            return Err(format!("multiple types named `{ident}`"));
        }
        let (impl_generics, data_type, decoder_type) = if has_lifetime {
            (quote!(<'a>), quote!(&'a [u8]), quote!(__impl::Decoder<'a>))
        } else {
            (quote!(), quote!(&[u8]), quote!(__impl::Decoder<'_>))
        };
        self.items.push(quote! {
            #[derive(Debug, Clone, PartialEq)]
            #item

            impl #impl_generics #ident #impl_generics {
                /// Decodes a value from its CBOR encoding.
                ///
                /// Returns an error if `data` isn't the encoding of a single valid value.
                pub fn decode(data: #data_type) -> ::core::result::Result<Self, DecodeError> {
                    let (value, consumed) = Self::decode_partial(data)?;
                    if consumed.get() != data.len() {
                        return Err(DecodeError::TrailingData);
                    }
                    Ok(value)
                }

                /// Decodes a value from the CBOR encoding found at the start of `data`.
                ///
                /// On success, returns the decoded value and the number of bytes of `data` that
                /// have been consumed. Bytes after these are ignored.
                pub fn decode_partial(
                    data: #data_type,
                ) -> ::core::result::Result<(Self, ::core::num::NonZero<usize>), DecodeError> {
                    let mut decoder = __impl::Decoder { remaining: data };
                    let value = Self::decode_from(&mut decoder)?;
                    let consumed = ::core::num::NonZero::new(data.len() - decoder.remaining.len())
                        .unwrap_or_else(|| {
                            // Every CBOR item starts with a head of at least one byte.
                            unreachable!()
                        });
                    Ok((value, consumed))
                }

                /// Decodes a value from the next CBOR item of `decoder`.
                fn decode_from(
                    decoder: &mut #decoder_type,
                ) -> ::core::result::Result<Self, DecodeError> {
                    #decode_body
                }

                /// Encodes the value to CBOR.
                ///
                /// `out` is called one or more times, and the concatenation of all the slices
                /// passed to it forms the encoding.
                pub fn encode_into(self, out: impl FnMut(&[u8])) {
                    self.encode_to(&mut __impl::Encoder { out });
                }

                /// Encodes the value to `encoder`.
                fn encode_to(self, encoder: &mut __impl::Encoder<impl FnMut(&[u8])>) {
                    #encode_body
                }
            }
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
