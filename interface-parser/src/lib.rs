//! Parser for interface definitions.
//!
//! An interface definition is a CDDL document, as defined in RFC 8610 and updated by RFC 9682,
//! with one additional constraint: the first rule of the document (the *root* rule) defines the
//! list of messages that can be exchanged over the interface.
//!
//! The root rule must be a type definition (`=`), and can be extended later in the document using
//! `/=`. Each of its type choices (separated with `/`) defines one possible message:
//!
//! - An array of exactly two items, such as `[ foo-request, foo-response ]`, defines a request
//!   and its response.
//! - Anything else defines a notification, which doesn't expect any response.
//!
//! ```
//! let definition = interface_parser::parse(b"
//!     message = [ hello-request, hello-response ] / goodbye
//!
//!     hello-request = [ tstr ]
//!     hello-response = {}
//!     goodbye = nil
//! ").unwrap();
//!
//! assert_eq!(definition.messages.len(), 2);
//! assert_eq!(definition.rules.len(), 4);
//! ```

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
use core::{fmt, ops::Range};

mod grammar;
mod tests;

/// Parses `interface` as a CDDL document describing an interface.
///
/// See the documentation of the crate for more information about the expected format.
pub fn parse(interface: &[u8]) -> Result<InterfaceDefinition, ParseError> {
    let document = core::str::from_utf8(interface).map_err(|err| ParseError {
        offset: err.valid_up_to(),
        kind: ParseErrorKind::InvalidUtf8,
    })?;

    let mut rules = match grammar::document(document) {
        Ok((_, rules)) => rules,
        Err(nom::Err::Error(err) | nom::Err::Failure(err)) => {
            return Err(ParseError {
                offset: document.len() - err.remaining.len(),
                kind: err.kind,
            });
        }
        Err(nom::Err::Incomplete(_)) => unreachable!(),
    };

    if rules.is_empty() {
        return Err(ParseError {
            offset: 0,
            kind: ParseErrorKind::EmptyDocument,
        });
    }

    // Build the list of all the names that can be referenced, starting with the standard prelude
    // of RFC 8610 (appendix D).
    let mut definitions = BTreeMap::<String, Definition>::new();
    for name in [
        "any",
        "uint",
        "nint",
        "int",
        "bstr",
        "bytes",
        "tstr",
        "text",
        "tdate",
        "time",
        "number",
        "biguint",
        "bignint",
        "bigint",
        "integer",
        "unsigned",
        "decfrac",
        "bigfloat",
        "eb64url",
        "eb64legacy",
        "eb16",
        "encoded-cbor",
        "uri",
        "b64url",
        "b64legacy",
        "regexp",
        "mime-message",
        "cbor-any",
        "float16",
        "float32",
        "float64",
        "float16-32",
        "float32-64",
        "float",
        "false",
        "true",
        "bool",
        "nil",
        "null",
        "undefined",
    ] {
        definitions.insert(
            String::from(name),
            Definition {
                kind: Kind::Type,
                num_generic_parameters: 0,
            },
        );
    }

    for rule in &rules {
        if rule.assignment != Assignment::Define {
            continue;
        }

        // A rule such as `foo = bar` is ambiguous, as `bar` can designate either a type or a
        // group. This is resolved below.
        let kind = match &rule.body {
            RuleBody::GroupEntry(_) => Kind::Group,
            RuleBody::Type(ty) if ty.single_name().is_some() => Kind::Unknown,
            RuleBody::Type(_) => Kind::Type,
        };

        let previous = definitions.insert(
            rule.name.clone(),
            Definition {
                kind,
                num_generic_parameters: rule.generic_parameters.len(),
            },
        );
        if previous.is_some() {
            return Err(ParseError {
                offset: rule.span.start,
                kind: ParseErrorKind::DuplicateRule(rule.name.clone()),
            });
        }
    }

    // Resolve the kind of the ambiguous rules by following the names they refer to.
    loop {
        let mut progress = false;

        for rule in &rules {
            if rule.assignment != Assignment::Define {
                continue;
            }
            let RuleBody::Type(ty) = &rule.body else {
                continue;
            };
            let Some(target) = ty.single_name() else {
                continue;
            };
            if definitions[&rule.name].kind != Kind::Unknown {
                continue;
            }

            // Generic parameters and undefined names are considered as types. Undefined names
            // are reported as errors later.
            let target_kind = if rule.generic_parameters.iter().any(|p| p == target) {
                Kind::Type
            } else {
                definitions.get(target).map_or(Kind::Type, |d| d.kind)
            };

            if target_kind != Kind::Unknown {
                definitions
                    .get_mut(&rule.name)
                    .unwrap_or_else(|| unreachable!())
                    .kind = target_kind;
                progress = true;
            }
        }

        if !progress {
            break;
        }
    }

    // Rules that are still ambiguous refer to each other in a cycle without ever reaching a
    // definition. Consider them as types.
    for definition in definitions.values_mut() {
        if definition.kind == Kind::Unknown {
            definition.kind = Kind::Type;
        }
    }

    for rule in &mut rules {
        if rule.assignment == Assignment::Define
            && definitions[&rule.name].kind == Kind::Group
            && let RuleBody::Type(ty) = &mut rule.body
        {
            let Some(Type1 {
                target:
                    Type2::Name {
                        name,
                        generic_arguments,
                    },
                ..
            }) = ty.choices.pop()
            else {
                // Only rules whose body is a single name can have been resolved as groups.
                unreachable!()
            };

            rule.body = RuleBody::GroupEntry(Box::new(GroupEntry {
                occurrence: None,
                kind: GroupEntryKind::GroupName {
                    name,
                    generic_arguments,
                },
            }));
        }
    }

    if rules[0].assignment != Assignment::Define
        || !rules[0].generic_parameters.is_empty()
        || !matches!(rules[0].body, RuleBody::Type(_))
    {
        return Err(ParseError {
            offset: rules[0].span.start,
            kind: ParseErrorKind::RootNotTypeRule,
        });
    }

    for rule in &mut rules {
        let result = match (&rule.assignment, &mut rule.body) {
            (Assignment::TypeChoiceExtension, _) | (Assignment::GroupChoiceExtension, _)
                if !definitions.contains_key(&rule.name) =>
            {
                Err(ParseErrorKind::UndefinedName(rule.name.clone()))
            }
            (Assignment::TypeChoiceExtension, _) if definitions[&rule.name].kind != Kind::Type => {
                Err(ParseErrorKind::GroupUsedAsType(rule.name.clone()))
            }
            (Assignment::GroupChoiceExtension, _)
                if definitions[&rule.name].kind != Kind::Group =>
            {
                Err(ParseErrorKind::TypeUsedAsGroup(rule.name.clone()))
            }
            (_, RuleBody::Type(ty)) => ty
                .choices
                .iter_mut()
                .try_for_each(|ty| check_type1(ty, &definitions, &rule.generic_parameters)),
            (_, RuleBody::GroupEntry(entry)) => {
                check_group_entry(entry, &definitions, &rule.generic_parameters)
            }
        };

        if let Err(kind) = result {
            return Err(ParseError {
                offset: rule.span.start,
                kind,
            });
        }
    }

    let mut messages = Vec::new();
    for rule in &rules {
        if rule.name != rules[0].name {
            continue;
        }

        // Rules with the same name as the root rule can only be the root rule itself or `/=`
        // extensions of it, as duplicates and `//=` have been rejected above.
        let RuleBody::Type(ty) = &rule.body else {
            unreachable!()
        };

        for choice in &ty.choices {
            let message = if choice.operator.is_none()
                && let Type2::Array(group) = &choice.target
                && let [single_choice] = &group.choices[..]
                && let [
                    GroupEntry {
                        occurrence: None,
                        kind:
                            GroupEntryKind::Member {
                                key: None | Some(MemberKey::Bareword(_)),
                                ty: request,
                            },
                    },
                    GroupEntry {
                        occurrence: None,
                        kind:
                            GroupEntryKind::Member {
                                key: None | Some(MemberKey::Bareword(_)),
                                ty: response,
                            },
                    },
                ] = &single_choice.entries[..]
            {
                Message::RequestResponse {
                    request: request.clone(),
                    response: response.clone(),
                    span: choice.span.clone(),
                }
            } else {
                Message::Notification {
                    ty: choice.clone(),
                    span: choice.span.clone(),
                }
            };

            messages.push(message);
        }
    }

    Ok(InterfaceDefinition { messages, rules })
}

/// Successfully-parsed interface definition.
#[derive(Debug, Clone, PartialEq)]
pub struct InterfaceDefinition {
    /// List of messages that can be sent over the interface, in the order in which they appear
    /// in the document.
    pub messages: Vec<Message>,

    /// List of all the rules of the document, in the order in which they appear in the document.
    ///
    /// The first rule is always the root rule, which defines the messages.
    pub rules: Vec<Rule>,
}

impl InterfaceDefinition {
    /// Returns a BLAKE3 hash of the structure of the interface.
    ///
    /// The hash takes into account all the rules of the document, in order, including their
    /// names, the names of their generic parameters, and their kind of assignment.
    ///
    /// The following differences don't modify the hash:
    ///
    /// - Comments, whitespaces, and more generally the location of the definitions within the
    ///   document.
    /// - Using `bytes` instead of `bstr`, `text` instead of `tstr`, or `null` instead of `nil`.
    /// - The order of type choices (`foo / bar`) and of group choices (`foo // bar`).
    pub fn hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(
            &u64::try_from(self.rules.len())
                .unwrap_or_else(|_| unreachable!())
                .to_le_bytes(),
        );
        for rule in &self.rules {
            rule.hash_into(&mut hasher);
        }
        *hasher.finalize().as_bytes()
    }
}

/// Message that can be sent over an interface.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// Request that expects a response.
    RequestResponse {
        /// Type of the request.
        request: Type,
        /// Type of the response.
        response: Type,
        /// Byte offsets within the document of the definition of this message.
        span: Range<usize>,
    },

    /// Notification, that doesn't expect any response.
    Notification {
        /// Type of the notification.
        ty: Type1,
        /// Byte offsets within the document of the definition of this message.
        span: Range<usize>,
    },
}

/// Rule of a CDDL document, such as `foo = uint`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// Name of the rule, such as `foo` in `foo = uint`.
    pub name: String,

    /// Names of the generic parameters, such as `["K", "V"]` in `foo<K, V> = { * K => V }`.
    pub generic_parameters: Vec<String>,

    /// Kind of assignment.
    pub assignment: Assignment,

    /// Body of the rule.
    pub body: RuleBody,

    /// Byte offsets within the document of the definition of this rule, from the start of the
    /// name of the rule to the end of its body, excluding any trailing whitespace or comment.
    pub span: Range<usize>,
}

/// Kind of assignment of a [`Rule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assignment {
    /// `=`
    Define,
    /// `/=`, which adds type choices to an already-defined type.
    TypeChoiceExtension,
    /// `//=`, which adds group choices to an already-defined group.
    GroupChoiceExtension,
}

/// Body of a [`Rule`].
#[derive(Debug, Clone, PartialEq)]
pub enum RuleBody {
    /// The rule defines a type.
    Type(Type),
    /// The rule defines a group.
    GroupEntry(Box<GroupEntry>),
}

/// List of type choices, such as `uint / tstr`.
#[derive(Debug, Clone, PartialEq)]
pub struct Type {
    /// List of choices. Never empty.
    pub choices: Vec<Type1>,
}

impl Type {
    /// If this type consists in a single name, such as `foo` or `foo<bar>`, returns this name.
    fn single_name(&self) -> Option<&str> {
        match &self.choices[..] {
            [
                Type1 {
                    target: Type2::Name { name, .. },
                    operator: None,
                    ..
                },
            ] => Some(name),
            _ => None,
        }
    }
}

/// Type optionally followed with an operator, such as `bytes .size 32` or `0..10`.
#[derive(Debug, Clone, PartialEq)]
pub struct Type1 {
    /// Type on the left side of the operator.
    pub target: Type2,

    /// Operator and right side of the operator, if any.
    pub operator: Option<(Operator, Type2)>,

    /// Byte offsets within the document of the definition of this type.
    pub span: Range<usize>,
}

/// Operator found in a [`Type1`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operator {
    /// `..`
    InclusiveRange,
    /// `...`
    ExclusiveRange,
    /// Control operator, such as `.size`. Contains the name without the leading dot.
    Control(String),
}

/// Individual type.
#[derive(Debug, Clone, PartialEq)]
pub enum Type2 {
    /// Literal value, such as `5` or `"hello"`.
    Value(Value),

    /// Reference to a type, such as `uint` or `foo<tstr>`.
    Name {
        /// Name of the type.
        name: String,
        /// Generic arguments. Empty if none.
        generic_arguments: Vec<Type1>,
    },

    /// Type between parentheses, such as `(uint / tstr)`.
    Parenthesized(Type),

    /// Map, such as `{ a: uint }`.
    Map(Group),

    /// Array, such as `[ uint, tstr ]`.
    Array(Group),

    /// Unwrapped type, such as `~foo`.
    Unwrap {
        /// Name of the type.
        name: String,
        /// Generic arguments. Empty if none.
        generic_arguments: Vec<Type1>,
    },

    /// Choice built from a group, such as `&( a: 1, b: 2 )`.
    ChoiceFromGroup(Group),

    /// Choice built from a group name, such as `&foo`.
    ChoiceFromGroupName {
        /// Name of the group.
        name: String,
        /// Generic arguments. Empty if none.
        generic_arguments: Vec<Type1>,
    },

    /// Tagged data item, such as `#6.32(tstr)`.
    Tag {
        /// Tag number. `None` if unspecified, in which case any tag is accepted.
        tag: Option<HeadNumber>,
        /// Type of the content of the tag.
        ty: Type,
    },

    /// Data item of a specific major type, such as `#1` or `#7.25`.
    MajorType {
        /// Major type.
        major: u8,
        /// Additional information. `None` if unspecified.
        argument: Option<HeadNumber>,
    },

    /// `#`, any data item.
    Any,
}

/// Number found after `#6.` or `#n.`.
#[derive(Debug, Clone, PartialEq)]
pub enum HeadNumber {
    /// Literal number, such as `#6.32`.
    Literal(u64),
    /// Number computed from a type, such as `#6.<uint>` (RFC 9682).
    Type(Box<Type>),
}

/// List of group choices, such as `a: uint // b: tstr`.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// List of choices. Never empty, but the choices themselves can be empty.
    pub choices: Vec<GroupChoice>,
}

/// Sequence of group entries.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupChoice {
    /// List of entries, in order.
    pub entries: Vec<GroupEntry>,
}

/// Entry of a group, such as `? a: uint`.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupEntry {
    /// Number of times this entry can occur. `None` if the entry must appear exactly once.
    pub occurrence: Option<Occurrence>,

    /// Content of the entry.
    pub kind: GroupEntryKind,
}

/// Content of a [`GroupEntry`].
#[derive(Debug, Clone, PartialEq)]
pub enum GroupEntryKind {
    /// Member, such as `a: uint` or `uint`.
    Member {
        /// Key of the member, if any.
        key: Option<MemberKey>,
        /// Type of the member.
        ty: Type,
    },

    /// Reference to a group, such as `foo` where `foo = ( a: uint )`.
    GroupName {
        /// Name of the group.
        name: String,
        /// Generic arguments. Empty if none.
        generic_arguments: Vec<Type1>,
    },

    /// Group between parentheses.
    Inline(Group),
}

/// Key of a [`GroupEntryKind::Member`].
#[derive(Debug, Clone, PartialEq)]
pub enum MemberKey {
    /// Bare word followed with `:`, such as `a:`. Equivalent to `"a" =>`.
    Bareword(String),

    /// Type followed with `=>`, such as `tstr =>`.
    Type {
        /// Type of the key.
        ty: Type1,
        /// `true` if the key is followed with `^`, such as `tstr ^ =>`.
        cut: bool,
    },

    /// Value followed with `:`, such as `1:`.
    Value(Value),
}

/// Number of times a [`GroupEntry`] can occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occurrence {
    /// Minimum number of times, inclusive.
    pub min: u64,
    /// Maximum number of times, inclusive. `None` if unbounded.
    pub max: Option<u64>,
}

/// Literal value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Integer. The parser accepts integers between `-(2^64 - 1)` and `2^64 - 1` inclusive.
    Integer(i128),
    /// Floating point number.
    Float(f64),
    /// Text string, with escape sequences decoded.
    Text(String),
    /// Byte string, with escape sequences and encodings decoded.
    Bytes(Vec<u8>),
}

/// Error that can happen when parsing an interface definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    offset: usize,
    kind: ParseErrorKind,
}

impl ParseError {
    /// Returns the byte offset within the document where the error happened.
    ///
    /// For errors related to a rule as a whole, this is the offset of the start of the rule.
    #[inline]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the kind of error.
    #[inline]
    pub fn kind(&self) -> &ParseErrorKind {
        &self.kind
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at offset {}", self.kind, self.offset)
    }
}

impl core::error::Error for ParseError {}

/// Kind of [`ParseError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// The document isn't valid UTF-8.
    InvalidUtf8,
    /// The document doesn't follow the CDDL grammar.
    Syntax,
    /// Invalid escape sequence in a string.
    InvalidEscape,
    /// Integer literal too large.
    IntegerOverflow,
    /// The document doesn't contain any rule.
    EmptyDocument,
    /// The root rule isn't a non-generic type definition.
    RootNotTypeRule,
    /// Reference to a name that isn't defined.
    UndefinedName(String),
    /// A rule with this name is defined multiple times.
    DuplicateRule(String),
    /// A group is used where a type is expected.
    GroupUsedAsType(String),
    /// A type is used where a group is expected.
    TypeUsedAsGroup(String),
    /// Wrong number of generic arguments when referring to this name.
    WrongGenericArity(String),
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseErrorKind::InvalidUtf8 => write!(f, "invalid UTF-8"),
            ParseErrorKind::Syntax => write!(f, "syntax error"),
            ParseErrorKind::InvalidEscape => write!(f, "invalid escape sequence"),
            ParseErrorKind::IntegerOverflow => write!(f, "integer literal too large"),
            ParseErrorKind::EmptyDocument => write!(f, "document doesn't contain any rule"),
            ParseErrorKind::RootNotTypeRule => {
                write!(f, "root rule must be a non-generic type definition")
            }
            ParseErrorKind::UndefinedName(name) => write!(f, "undefined name `{name}`"),
            ParseErrorKind::DuplicateRule(name) => {
                write!(f, "rule `{name}` defined multiple times")
            }
            ParseErrorKind::GroupUsedAsType(name) => write!(f, "group `{name}` used as a type"),
            ParseErrorKind::TypeUsedAsGroup(name) => write!(f, "type `{name}` used as a group"),
            ParseErrorKind::WrongGenericArity(name) => {
                write!(f, "wrong number of generic arguments for `{name}`")
            }
        }
    }
}

/// Information about a name that can be referenced in the document.
struct Definition {
    kind: Kind,
    num_generic_parameters: usize,
}

/// Whether a name designates a type or a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Type,
    Group,
    /// Not determined yet.
    Unknown,
}

/// Verifies that all the names referenced by the given type are defined and used appropriately.
fn check_type1(
    ty: &mut Type1,
    definitions: &BTreeMap<String, Definition>,
    generic_parameters: &[String],
) -> Result<(), ParseErrorKind> {
    for target in core::iter::once(&mut ty.target).chain(ty.operator.as_mut().map(|(_, t)| t)) {
        match target {
            Type2::Value(_) | Type2::Any => {}
            Type2::Name {
                name,
                generic_arguments,
            }
            | Type2::Unwrap {
                name,
                generic_arguments,
            } => check_reference(
                name,
                generic_arguments,
                Kind::Type,
                definitions,
                generic_parameters,
            )?,
            Type2::ChoiceFromGroupName {
                name,
                generic_arguments,
            } => check_reference(
                name,
                generic_arguments,
                Kind::Group,
                definitions,
                generic_parameters,
            )?,
            Type2::Parenthesized(ty) => {
                for ty in &mut ty.choices {
                    check_type1(ty, definitions, generic_parameters)?;
                }
            }
            Type2::Map(group) | Type2::Array(group) | Type2::ChoiceFromGroup(group) => {
                for choice in &mut group.choices {
                    for entry in &mut choice.entries {
                        check_group_entry(entry, definitions, generic_parameters)?;
                    }
                }
            }
            Type2::Tag { tag, ty } => {
                if let Some(HeadNumber::Type(tag)) = tag {
                    for ty in &mut tag.choices {
                        check_type1(ty, definitions, generic_parameters)?;
                    }
                }
                for ty in &mut ty.choices {
                    check_type1(ty, definitions, generic_parameters)?;
                }
            }
            Type2::MajorType { argument, .. } => {
                if let Some(HeadNumber::Type(argument)) = argument {
                    for ty in &mut argument.choices {
                        check_type1(ty, definitions, generic_parameters)?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Verifies that all the names referenced by the given group entry are defined and used
/// appropriately.
///
/// Members that consist only in the name of a group are converted to
/// [`GroupEntryKind::GroupName`], as the grammar alone can't distinguish them from types.
fn check_group_entry(
    entry: &mut GroupEntry,
    definitions: &BTreeMap<String, Definition>,
    generic_parameters: &[String],
) -> Result<(), ParseErrorKind> {
    if let GroupEntryKind::Member { key: None, ty } = &mut entry.kind
        && let Some(name) = ty.single_name()
        && !generic_parameters.iter().any(|p| p == name)
        && definitions.get(name).is_some_and(|d| d.kind == Kind::Group)
    {
        let Some(Type1 {
            target:
                Type2::Name {
                    name,
                    generic_arguments,
                },
            ..
        }) = ty.choices.pop()
        else {
            unreachable!()
        };
        entry.kind = GroupEntryKind::GroupName {
            name,
            generic_arguments,
        };
    }

    match &mut entry.kind {
        GroupEntryKind::Member { key, ty } => {
            if let Some(MemberKey::Type { ty, .. }) = key {
                check_type1(ty, definitions, generic_parameters)?;
            }
            for ty in &mut ty.choices {
                check_type1(ty, definitions, generic_parameters)?;
            }
        }
        GroupEntryKind::GroupName {
            name,
            generic_arguments,
        } => check_reference(
            name,
            generic_arguments,
            Kind::Group,
            definitions,
            generic_parameters,
        )?,
        GroupEntryKind::Inline(group) => {
            for choice in &mut group.choices {
                for entry in &mut choice.entries {
                    check_group_entry(entry, definitions, generic_parameters)?;
                }
            }
        }
    }

    Ok(())
}

/// Verifies that `name` is defined, is of the expected kind, and that the number of generic
/// arguments matches the definition. Also checks the generic arguments themselves.
fn check_reference(
    name: &str,
    generic_arguments: &mut [Type1],
    expected_kind: Kind,
    definitions: &BTreeMap<String, Definition>,
    generic_parameters: &[String],
) -> Result<(), ParseErrorKind> {
    if generic_parameters.iter().any(|p| p == name) {
        if !generic_arguments.is_empty() {
            return Err(ParseErrorKind::WrongGenericArity(String::from(name)));
        }
        return Ok(());
    }

    let Some(definition) = definitions.get(name) else {
        return Err(ParseErrorKind::UndefinedName(String::from(name)));
    };

    match (definition.kind, expected_kind) {
        (Kind::Group, Kind::Type) => {
            return Err(ParseErrorKind::GroupUsedAsType(String::from(name)));
        }
        (Kind::Type, Kind::Group) => {
            return Err(ParseErrorKind::TypeUsedAsGroup(String::from(name)));
        }
        _ => {}
    }

    if definition.num_generic_parameters != generic_arguments.len() {
        return Err(ParseErrorKind::WrongGenericArity(String::from(name)));
    }

    for argument in generic_arguments {
        check_type1(argument, definitions, generic_parameters)?;
    }

    Ok(())
}

/// Writes to `hasher` the given name, taking into account the aliases of the standard prelude.
fn hash_name(hasher: &mut blake3::Hasher, name: &str) {
    let name = match name {
        "bytes" => "bstr",
        "text" => "tstr",
        "null" => "nil",
        other => other,
    };
    hasher.update(
        &u64::try_from(name.len())
            .unwrap_or_else(|_| unreachable!())
            .to_le_bytes(),
    );
    hasher.update(name.as_bytes());
}

/// Writes to `hasher` the given name and list of generic arguments.
fn hash_reference(hasher: &mut blake3::Hasher, name: &str, generic_arguments: &[Type1]) {
    hash_name(hasher, name);
    hasher.update(
        &u64::try_from(generic_arguments.len())
            .unwrap_or_else(|_| unreachable!())
            .to_le_bytes(),
    );
    for argument in generic_arguments {
        argument.hash_into(hasher);
    }
}

impl Rule {
    /// Writes the rule to `hasher`. See [`InterfaceDefinition::hash`].
    fn hash_into(&self, hasher: &mut blake3::Hasher) {
        hash_name(hasher, &self.name);
        hasher.update(
            &u64::try_from(self.generic_parameters.len())
                .unwrap_or_else(|_| unreachable!())
                .to_le_bytes(),
        );
        for parameter in &self.generic_parameters {
            hash_name(hasher, parameter);
        }
        hasher.update(&[match self.assignment {
            Assignment::Define => 0,
            Assignment::TypeChoiceExtension => 1,
            Assignment::GroupChoiceExtension => 2,
        }]);
        match &self.body {
            RuleBody::Type(ty) => {
                hasher.update(&[0]);
                ty.hash_into(hasher);
            }
            RuleBody::GroupEntry(entry) => {
                hasher.update(&[1]);
                entry.hash_into(hasher);
            }
        }
    }
}

impl Type {
    /// Writes the type to `hasher`, in a way that doesn't depend on the order of the choices.
    fn hash_into(&self, hasher: &mut blake3::Hasher) {
        let mut choices = self
            .choices
            .iter()
            .map(|choice| {
                let mut choice_hasher = blake3::Hasher::new();
                choice.hash_into(&mut choice_hasher);
                *choice_hasher.finalize().as_bytes()
            })
            .collect::<Vec<[u8; 32]>>();
        choices.sort_unstable();
        hasher.update(
            &u64::try_from(choices.len())
                .unwrap_or_else(|_| unreachable!())
                .to_le_bytes(),
        );
        for choice in &choices {
            hasher.update(choice);
        }
    }
}

impl Type1 {
    /// Writes the type to `hasher`. See [`InterfaceDefinition::hash`].
    fn hash_into(&self, hasher: &mut blake3::Hasher) {
        self.target.hash_into(hasher);
        match &self.operator {
            None => {
                hasher.update(&[0]);
            }
            Some((Operator::InclusiveRange, right)) => {
                hasher.update(&[1]);
                right.hash_into(hasher);
            }
            Some((Operator::ExclusiveRange, right)) => {
                hasher.update(&[2]);
                right.hash_into(hasher);
            }
            Some((Operator::Control(name), right)) => {
                hasher.update(&[3]);
                hasher.update(
                    &u64::try_from(name.len())
                        .unwrap_or_else(|_| unreachable!())
                        .to_le_bytes(),
                );
                hasher.update(name.as_bytes());
                right.hash_into(hasher);
            }
        }
    }
}

impl Type2 {
    /// Writes the type to `hasher`. See [`InterfaceDefinition::hash`].
    fn hash_into(&self, hasher: &mut blake3::Hasher) {
        match self {
            Type2::Value(value) => {
                hasher.update(&[0]);
                value.hash_into(hasher);
            }
            Type2::Name {
                name,
                generic_arguments,
            } => {
                hasher.update(&[1]);
                hash_reference(hasher, name, generic_arguments);
            }
            Type2::Parenthesized(ty) => {
                hasher.update(&[2]);
                ty.hash_into(hasher);
            }
            Type2::Map(group) => {
                hasher.update(&[3]);
                group.hash_into(hasher);
            }
            Type2::Array(group) => {
                hasher.update(&[4]);
                group.hash_into(hasher);
            }
            Type2::Unwrap {
                name,
                generic_arguments,
            } => {
                hasher.update(&[5]);
                hash_reference(hasher, name, generic_arguments);
            }
            Type2::ChoiceFromGroup(group) => {
                hasher.update(&[6]);
                group.hash_into(hasher);
            }
            Type2::ChoiceFromGroupName {
                name,
                generic_arguments,
            } => {
                hasher.update(&[7]);
                hash_reference(hasher, name, generic_arguments);
            }
            Type2::Tag { tag, ty } => {
                hasher.update(&[8]);
                match tag {
                    None => {
                        hasher.update(&[0]);
                    }
                    Some(tag) => {
                        hasher.update(&[1]);
                        tag.hash_into(hasher);
                    }
                }
                ty.hash_into(hasher);
            }
            Type2::MajorType { major, argument } => {
                hasher.update(&[9, *major]);
                match argument {
                    None => {
                        hasher.update(&[0]);
                    }
                    Some(argument) => {
                        hasher.update(&[1]);
                        argument.hash_into(hasher);
                    }
                }
            }
            Type2::Any => {
                hasher.update(&[10]);
            }
        }
    }
}

impl HeadNumber {
    /// Writes the number to `hasher`. See [`InterfaceDefinition::hash`].
    fn hash_into(&self, hasher: &mut blake3::Hasher) {
        match self {
            HeadNumber::Literal(number) => {
                hasher.update(&[0]);
                hasher.update(&number.to_le_bytes());
            }
            HeadNumber::Type(ty) => {
                hasher.update(&[1]);
                ty.hash_into(hasher);
            }
        }
    }
}

impl Group {
    /// Writes the group to `hasher`, in a way that doesn't depend on the order of the choices.
    fn hash_into(&self, hasher: &mut blake3::Hasher) {
        let mut choices = self
            .choices
            .iter()
            .map(|choice| {
                let mut choice_hasher = blake3::Hasher::new();
                choice_hasher.update(
                    &u64::try_from(choice.entries.len())
                        .unwrap_or_else(|_| unreachable!())
                        .to_le_bytes(),
                );
                for entry in &choice.entries {
                    entry.hash_into(&mut choice_hasher);
                }
                *choice_hasher.finalize().as_bytes()
            })
            .collect::<Vec<[u8; 32]>>();
        choices.sort_unstable();
        hasher.update(
            &u64::try_from(choices.len())
                .unwrap_or_else(|_| unreachable!())
                .to_le_bytes(),
        );
        for choice in &choices {
            hasher.update(choice);
        }
    }
}

impl GroupEntry {
    /// Writes the entry to `hasher`. See [`InterfaceDefinition::hash`].
    fn hash_into(&self, hasher: &mut blake3::Hasher) {
        match &self.occurrence {
            None => {
                hasher.update(&[0]);
            }
            Some(Occurrence { min, max: None }) => {
                hasher.update(&[1]);
                hasher.update(&min.to_le_bytes());
            }
            Some(Occurrence {
                min,
                max: Some(max),
            }) => {
                hasher.update(&[2]);
                hasher.update(&min.to_le_bytes());
                hasher.update(&max.to_le_bytes());
            }
        }

        match &self.kind {
            GroupEntryKind::Member { key, ty } => {
                hasher.update(&[0]);
                match key {
                    None => {
                        hasher.update(&[0]);
                    }
                    Some(MemberKey::Bareword(word)) => {
                        hasher.update(&[1]);
                        hasher.update(
                            &u64::try_from(word.len())
                                .unwrap_or_else(|_| unreachable!())
                                .to_le_bytes(),
                        );
                        hasher.update(word.as_bytes());
                    }
                    Some(MemberKey::Type { ty, cut }) => {
                        hasher.update(&[2, u8::from(*cut)]);
                        ty.hash_into(hasher);
                    }
                    Some(MemberKey::Value(value)) => {
                        hasher.update(&[3]);
                        value.hash_into(hasher);
                    }
                }
                ty.hash_into(hasher);
            }
            GroupEntryKind::GroupName {
                name,
                generic_arguments,
            } => {
                hasher.update(&[1]);
                hash_reference(hasher, name, generic_arguments);
            }
            GroupEntryKind::Inline(group) => {
                hasher.update(&[2]);
                group.hash_into(hasher);
            }
        }
    }
}

impl Value {
    /// Writes the value to `hasher`. See [`InterfaceDefinition::hash`].
    fn hash_into(&self, hasher: &mut blake3::Hasher) {
        match self {
            Value::Integer(value) => {
                hasher.update(&[0]);
                hasher.update(&value.to_le_bytes());
            }
            Value::Float(value) => {
                hasher.update(&[1]);
                hasher.update(&value.to_bits().to_le_bytes());
            }
            Value::Text(text) => {
                hasher.update(&[2]);
                hasher.update(
                    &u64::try_from(text.len())
                        .unwrap_or_else(|_| unreachable!())
                        .to_le_bytes(),
                );
                hasher.update(text.as_bytes());
            }
            Value::Bytes(bytes) => {
                hasher.update(&[3]);
                hasher.update(
                    &u64::try_from(bytes.len())
                        .unwrap_or_else(|_| unreachable!())
                        .to_le_bytes(),
                );
                hasher.update(bytes);
            }
        }
    }
}
