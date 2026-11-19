//! CDDL grammar, as defined in appendix B of RFC 8610 and updated by RFC 9682.
//!
//! Most parsing functions take as parameter both the entire document and the remaining input,
//! in order to be able to compute spans.

use crate::{
    Assignment, Group, GroupChoice, GroupEntry, GroupEntryKind, HeadNumber, MemberKey, Occurrence,
    Operator, ParseErrorKind, Rule, RuleBody, Type, Type1, Type2, Value,
};

use alloc::{boxed::Box, string::String, vec::Vec};
use nom::{
    IResult, Parser as _,
    branch::alt,
    bytes::complete::{tag, take_while, take_while1},
    character::complete::{digit1, hex_digit1, satisfy},
    combinator::{eof, not, opt, peek, recognize},
    multi::{many0, separated_list1},
    sequence::{delimited, preceded, terminated},
};

/// Error produced by the parsing functions of this module.
#[derive(Debug)]
pub(crate) struct Error<'a> {
    /// Remaining input when the error happened.
    pub(crate) remaining: &'a str,
    pub(crate) kind: ParseErrorKind,
}

impl<'a> nom::error::ParseError<&'a str> for Error<'a> {
    fn from_error_kind(input: &'a str, _: nom::error::ErrorKind) -> Self {
        Error {
            remaining: input,
            kind: ParseErrorKind::Syntax,
        }
    }

    fn append(_: &'a str, _: nom::error::ErrorKind, other: Self) -> Self {
        other
    }

    fn or(self, other: Self) -> Self {
        // Keep the error that happened the furthest in the document, as it is most likely the
        // most relevant.
        if other.remaining.len() < self.remaining.len() {
            other
        } else {
            self
        }
    }
}

/// Parses an entire document into a list of rules.
pub(crate) fn document(doc: &str) -> IResult<&str, Vec<Rule>, Error<'_>> {
    let mut rules = Vec::new();
    let (mut input, ()) = s(doc)?;

    while !input.is_empty() {
        let rule_start = doc.len() - input.len();
        let (rest, (name, generic_parameters, assignment)) = rule_head(input)?;
        let (rest, ()) = s(rest)?;

        let (rest, body) = match assignment {
            Assignment::TypeChoiceExtension => {
                ty(doc, rest).map(|(rest, ty)| (rest, RuleBody::Type(ty)))?
            }
            Assignment::GroupChoiceExtension => group_entry(doc, rest)
                .map(|(rest, entry)| (rest, RuleBody::GroupEntry(Box::new(entry))))?,
            Assignment::Define => {
                // The body is first parsed as a type. The type must then be followed with the
                // end of the document or the start of a new rule, otherwise the body is parsed
                // again as a group entry.
                let as_type = ty(doc, rest).and_then(|(after, parsed)| {
                    (s, alt((eof.map(|_| ()), peek(rule_head).map(|_| ()))))
                        .parse(after)
                        .map(|_| (after, parsed))
                });

                match as_type {
                    Ok((rest, ty)) => (rest, RuleBody::Type(ty)),
                    Err(nom::Err::Error(type_error)) => match group_entry(doc, rest) {
                        Ok((rest, entry)) => (rest, RuleBody::GroupEntry(Box::new(entry))),
                        Err(nom::Err::Error(group_error)) => {
                            return Err(nom::Err::Error(nom::error::ParseError::or(
                                type_error,
                                group_error,
                            )));
                        }
                        Err(err) => return Err(err),
                    },
                    Err(err) => return Err(err),
                }
            }
        };

        rules.push(Rule {
            name: String::from(name),
            generic_parameters: generic_parameters
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
            assignment,
            body,
            span: rule_start..(doc.len() - rest.len()),
        });

        let (rest, ()) = s(rest)?;
        input = rest;
    }

    Ok((input, rules))
}

/// Parses the start of a rule: its name, generic parameters, and assignment operator.
fn rule_head(input: &str) -> IResult<&str, (&str, Vec<&str>, Assignment), Error<'_>> {
    (
        id,
        opt(delimited(
            (tag("<"), s),
            separated_list1((tag(","), s), terminated(id, s)),
            tag(">"),
        ))
        .map(Option::unwrap_or_default),
        s,
        alt((
            nom::combinator::value(Assignment::GroupChoiceExtension, tag("//=")),
            nom::combinator::value(Assignment::TypeChoiceExtension, tag("/=")),
            nom::combinator::value(Assignment::Define, tag("=")),
        )),
    )
        .map(|(name, generic_parameters, (), assignment)| (name, generic_parameters, assignment))
        .parse(input)
}

/// Skips whitespaces and comments. Never fails.
fn s(mut input: &str) -> IResult<&str, (), Error<'_>> {
    loop {
        if let Some(rest) = input.strip_prefix([' ', '\t', '\n']) {
            input = rest;
        } else if let Some(rest) = input.strip_prefix("\r\n") {
            input = rest;
        } else if let Some(comment) = input.strip_prefix(';') {
            input = comment.find('\n').map_or("", |pos| &comment[pos + 1..]);
        } else {
            return Ok((input, ()));
        }
    }
}

/// Parses an identifier.
fn id(input: &str) -> IResult<&str, &str, Error<'_>> {
    recognize((
        satisfy(|c| c.is_ascii_alphabetic() || matches!(c, '@' | '_' | '$')),
        many0((
            take_while(|c| matches!(c, '-' | '.')),
            satisfy(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '_' | '$')),
        )),
    ))
    .parse(input)
}

/// Parses an optional list of generic arguments, such as `<uint, tstr>`. Returns an empty list
/// if there isn't any.
fn generic_arguments<'a>(doc: &'a str, input: &'a str) -> IResult<&'a str, Vec<Type1>, Error<'a>> {
    opt(delimited(
        (tag("<"), s),
        separated_list1((s, tag(","), s), |i| type1(doc, i)),
        (s, tag(">")),
    ))
    .map(Option::unwrap_or_default)
    .parse(input)
}

/// Parses a list of type choices, such as `uint / tstr`.
fn ty<'a>(doc: &'a str, input: &'a str) -> IResult<&'a str, Type, Error<'a>> {
    separated_list1((s, tag("/"), not(tag("/")), s), |i| type1(doc, i))
        .map(|choices| Type { choices })
        .parse(input)
}

/// Parses a type optionally followed with an operator, such as `bytes .size 32`.
fn type1<'a>(doc: &'a str, input: &'a str) -> IResult<&'a str, Type1, Error<'a>> {
    let start = doc.len() - input.len();
    let (input, target) = type2(doc, input)?;
    let (input, operator) = opt((
        s,
        alt((
            nom::combinator::value(Operator::ExclusiveRange, tag("...")),
            nom::combinator::value(Operator::InclusiveRange, tag("..")),
            preceded(tag("."), id).map(|name| Operator::Control(String::from(name))),
        )),
        s,
        |i| type2(doc, i),
    )
        .map(|((), operator, (), target)| (operator, target)))
    .parse(input)?;

    Ok((
        input,
        Type1 {
            target,
            operator,
            span: start..(doc.len() - input.len()),
        },
    ))
}

/// Parses an individual type.
fn type2<'a>(doc: &'a str, input: &'a str) -> IResult<&'a str, Type2, Error<'a>> {
    alt((
        // Literals must be tried before names, as byte strings can start with `h` or `b64`.
        literal.map(Type2::Value),
        (id, |i| generic_arguments(doc, i)).map(|(name, generic_arguments)| Type2::Name {
            name: String::from(name),
            generic_arguments,
        }),
        delimited((tag("("), s), |i| ty(doc, i), (s, tag(")"))).map(Type2::Parenthesized),
        delimited((tag("{"), s), |i| group(doc, i), (s, tag("}"))).map(Type2::Map),
        delimited((tag("["), s), |i| group(doc, i), (s, tag("]"))).map(Type2::Array),
        preceded((tag("~"), s), (id, |i| generic_arguments(doc, i))).map(
            |(name, generic_arguments)| Type2::Unwrap {
                name: String::from(name),
                generic_arguments,
            },
        ),
        delimited((tag("&"), s, tag("("), s), |i| group(doc, i), (s, tag(")")))
            .map(Type2::ChoiceFromGroup),
        preceded((tag("&"), s), (id, |i| generic_arguments(doc, i))).map(
            |(name, generic_arguments)| Type2::ChoiceFromGroupName {
                name: String::from(name),
                generic_arguments,
            },
        ),
        (
            tag("#6"),
            opt(preceded(tag("."), |i| head_number(doc, i))),
            tag("("),
            s,
            |i| ty(doc, i),
            s,
            tag(")"),
        )
            .map(|(_, number, _, (), ty, (), _)| Type2::Tag { tag: number, ty }),
        (
            tag("#"),
            satisfy(|c| c.is_ascii_digit()),
            opt(preceded(tag("."), |i| head_number(doc, i))),
        )
            .map(|(_, major, argument)| Type2::MajorType {
                major: major
                    .to_digit(10)
                    .and_then(|d| u8::try_from(d).ok())
                    .unwrap_or_else(|| unreachable!()),
                argument,
            }),
        nom::combinator::value(Type2::Any, tag("#")),
    ))
    .parse(input)
}

/// Parses the number found after `#6.` or `#n.`, such as `32` or `<uint>`.
fn head_number<'a>(doc: &'a str, input: &'a str) -> IResult<&'a str, HeadNumber, Error<'a>> {
    alt((
        uint.map(HeadNumber::Literal),
        delimited((tag("<"), s), |i| ty(doc, i), (s, tag(">")))
            .map(|ty| HeadNumber::Type(Box::new(ty))),
    ))
    .parse(input)
}

/// Parses a list of group choices, such as `a: uint // b: tstr`.
fn group<'a>(doc: &'a str, input: &'a str) -> IResult<&'a str, Group, Error<'a>> {
    separated_list1(
        (s, tag("//"), s),
        many0(terminated(|i| group_entry(doc, i), (s, opt((tag(","), s)))))
            .map(|entries| GroupChoice { entries }),
    )
    .map(|choices| Group { choices })
    .parse(input)
}

/// Parses a group entry, such as `? a: uint`.
fn group_entry<'a>(doc: &'a str, input: &'a str) -> IResult<&'a str, GroupEntry, Error<'a>> {
    let (input, occurrence) = opt(terminated(
        alt((
            nom::combinator::value(
                Occurrence {
                    min: 0,
                    max: Some(1),
                },
                tag("?"),
            ),
            nom::combinator::value(Occurrence { min: 1, max: None }, tag("+")),
            (opt(uint), tag("*"), opt(uint)).map(|(min, _, max)| Occurrence {
                min: min.unwrap_or(0),
                max,
            }),
        )),
        s,
    ))
    .parse(input)?;

    // Member key in the form `bareword:` or `value:`.
    if let Ok((input, key)) = terminated(
        alt((
            id.map(|name| MemberKey::Bareword(String::from(name))),
            literal.map(MemberKey::Value),
        )),
        (s, tag(":")),
    )
    .parse(input)
    {
        let (input, ()) = s(input)?;
        let (input, ty) = ty(doc, input)?;
        return Ok((
            input,
            GroupEntry {
                occurrence,
                kind: GroupEntryKind::Member { key: Some(key), ty },
            },
        ));
    }

    // At this point, the entry is either `type =>`, a type, or a group between parentheses.
    // In order to avoid parsing the same type multiple times, a `type1` is parsed then the
    // parsing continues depending on what follows.
    match type1(doc, input) {
        Ok((rest, first)) => {
            if let Ok((rest, cut)) = (s, opt((tag("^"), s)), tag("=>"), s)
                .map(|((), cut, _, ())| cut.is_some())
                .parse(rest)
            {
                let (rest, ty) = ty(doc, rest)?;
                return Ok((
                    rest,
                    GroupEntry {
                        occurrence,
                        kind: GroupEntryKind::Member {
                            key: Some(MemberKey::Type { ty: first, cut }),
                            ty,
                        },
                    },
                ));
            }

            let (rest, other_choices) =
                many0(preceded((s, tag("/"), not(tag("/")), s), |i| type1(doc, i))).parse(rest)?;

            let mut choices = Vec::with_capacity(1 + other_choices.len());
            choices.push(first);
            choices.extend(other_choices);

            Ok((
                rest,
                GroupEntry {
                    occurrence,
                    kind: GroupEntryKind::Member {
                        key: None,
                        ty: Type { choices },
                    },
                },
            ))
        }
        Err(nom::Err::Error(type_error)) => {
            match delimited((tag("("), s), |i| group(doc, i), (s, tag(")"))).parse(input) {
                Ok((rest, group)) => Ok((
                    rest,
                    GroupEntry {
                        occurrence,
                        kind: GroupEntryKind::Inline(group),
                    },
                )),
                Err(nom::Err::Error(group_error)) => Err(nom::Err::Error(
                    nom::error::ParseError::or(type_error, group_error),
                )),
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

/// Parses an unsigned integer, in decimal, hexadecimal (`0x`), or binary (`0b`).
fn uint(input: &str) -> IResult<&str, u64, Error<'_>> {
    let (rest, (radix, digits)) = alt((
        preceded(tag("0x"), hex_digit1).map(|digits| (16, digits)),
        preceded(tag("0b"), take_while1(|c| matches!(c, '0' | '1'))).map(|digits| (2, digits)),
        digit1.map(|digits| (10, digits)),
    ))
    .parse(input)?;

    match u64::from_str_radix(digits, radix) {
        Ok(value) => Ok((rest, value)),
        Err(_) => Err(nom::Err::Failure(Error {
            remaining: input,
            kind: ParseErrorKind::IntegerOverflow,
        })),
    }
}

/// Parses a literal value: number, text string, or byte string.
fn literal(input: &str) -> IResult<&str, Value, Error<'_>> {
    // Text string.
    if let Some(mut rest) = input.strip_prefix('"') {
        let mut out = String::new();
        loop {
            let mut chars = rest.chars();
            match chars.next() {
                Some('"') => return Ok((chars.as_str(), Value::Text(out))),
                Some('\\') => {
                    let (after, c) = escape(rest)?;
                    out.push(c);
                    rest = after;
                }
                Some(c) if !c.is_control() => {
                    out.push(c);
                    rest = chars.as_str();
                }
                _ => {
                    return Err(nom::Err::Failure(Error {
                        remaining: rest,
                        kind: ParseErrorKind::Syntax,
                    }));
                }
            }
        }
    }

    // Byte string in the form `'...'`.
    if let Some(mut rest) = input.strip_prefix('\'') {
        let mut out = String::new();
        loop {
            let mut chars = rest.chars();
            match chars.next() {
                Some('\'') => return Ok((chars.as_str(), Value::Bytes(out.into_bytes()))),
                Some('\\') if rest.starts_with("\\'") => {
                    out.push('\'');
                    rest = &rest[2..];
                }
                Some('\\') => {
                    let (after, c) = escape(rest)?;
                    out.push(c);
                    rest = after;
                }
                Some(c) if !c.is_control() || c == '\n' => {
                    out.push(c);
                    rest = chars.as_str();
                }
                Some('\r') if rest.starts_with("\r\n") => {
                    out.push_str("\r\n");
                    rest = &rest[2..];
                }
                _ => {
                    return Err(nom::Err::Failure(Error {
                        remaining: rest,
                        kind: ParseErrorKind::Syntax,
                    }));
                }
            }
        }
    }

    // Byte string in the form `h'...'`. Whitespaces and comments are allowed between digits.
    if let Some(mut rest) = input.strip_prefix("h'") {
        let mut out = Vec::new();
        let mut high_nibble = None;
        loop {
            (rest, ()) = s(rest)?;
            let mut chars = rest.chars();
            match (chars.next(), high_nibble) {
                (Some('\''), None) => return Ok((chars.as_str(), Value::Bytes(out))),
                (Some(c), _) if c.is_ascii_hexdigit() => {
                    let nibble = c.to_digit(16).unwrap_or_else(|| unreachable!());
                    if let Some(high) = high_nibble.take() {
                        out.push(
                            u8::try_from((high << 4) | nibble).unwrap_or_else(|_| unreachable!()),
                        );
                    } else {
                        high_nibble = Some(nibble);
                    }
                    rest = chars.as_str();
                }
                _ => {
                    return Err(nom::Err::Failure(Error {
                        remaining: rest,
                        kind: ParseErrorKind::Syntax,
                    }));
                }
            }
        }
    }

    // Byte string in the form `b64'...'`. Both the standard and URL-safe alphabets are accepted,
    // and padding is optional. Whitespaces and comments are allowed between characters.
    if let Some(mut rest) = input.strip_prefix("b64'") {
        let mut out = Vec::new();
        let mut buffer = 0u32;
        let mut num_bits = 0u32;
        loop {
            (rest, ()) = s(rest)?;
            let mut chars = rest.chars();
            let sextet = match chars.next() {
                Some('\'') => return Ok((chars.as_str(), Value::Bytes(out))),
                Some('=') => {
                    rest = chars.as_str();
                    continue;
                }
                Some(c @ 'A'..='Z') => u32::from(c) - u32::from('A'),
                Some(c @ 'a'..='z') => u32::from(c) - u32::from('a') + 26,
                Some(c @ '0'..='9') => u32::from(c) - u32::from('0') + 52,
                Some('+' | '-') => 62,
                Some('/' | '_') => 63,
                _ => {
                    return Err(nom::Err::Failure(Error {
                        remaining: rest,
                        kind: ParseErrorKind::Syntax,
                    }));
                }
            };
            rest = chars.as_str();

            buffer = (buffer << 6) | sextet;
            num_bits += 6;
            if num_bits >= 8 {
                num_bits -= 8;
                out.push(u8::try_from(buffer >> num_bits).unwrap_or_else(|_| unreachable!()));
                buffer &= (1 << num_bits) - 1;
            }
        }
    }

    // Numbers.
    let (rest, negative) = opt(tag::<_, _, Error>("-"))
        .map(|sign| sign.is_some())
        .parse(input)?;

    // Hexadecimal floating point number, such as `0x1.8p3`.
    if let Ok((rest, (_, integer_digits, fraction_digits, _, exponent))) = (
        tag::<_, _, Error>("0x"),
        hex_digit1,
        opt(preceded(tag("."), hex_digit1)).map(Option::unwrap_or_default),
        tag("p"),
        recognize((opt(alt((tag("+"), tag("-")))), digit1)),
    )
        .parse(rest)
    {
        let mut value = 0.0f64;
        for digit in integer_digits.chars().chain(fraction_digits.chars()) {
            value = value * 16.0 + f64::from(digit.to_digit(16).unwrap_or_else(|| unreachable!()));
        }

        let Some(mut exponent) = exponent.parse::<i64>().ok().and_then(|e| {
            e.checked_sub(i64::try_from(fraction_digits.len()).ok()?.checked_mul(4)?)
        }) else {
            return Err(nom::Err::Failure(Error {
                remaining: input,
                kind: ParseErrorKind::IntegerOverflow,
            }));
        };

        while exponent > 0 && value.is_finite() && value != 0.0 {
            value *= 2.0;
            exponent -= 1;
        }
        while exponent < 0 && value != 0.0 {
            value /= 2.0;
            exponent += 1;
        }

        return Ok((rest, Value::Float(if negative { -value } else { value })));
    }

    // Decimal floating point number, such as `1.5` or `2e10`.
    if let Ok((rest, text)) = recognize((
        digit1::<_, Error>,
        alt((
            (
                tag("."),
                digit1,
                opt((tag("e"), opt(alt((tag("+"), tag("-")))), digit1)),
            )
                .map(|_| ()),
            (tag("e"), opt(alt((tag("+"), tag("-")))), digit1).map(|_| ()),
        )),
    ))
    .parse(rest)
    {
        let value = text.parse::<f64>().unwrap_or_else(|_| unreachable!());
        return Ok((rest, Value::Float(if negative { -value } else { value })));
    }

    let (rest, value) = uint(rest)?;
    let value = i128::from(value);
    Ok((rest, Value::Integer(if negative { -value } else { value })))
}

/// Parses an escape sequence in a string, starting with the backslash.
fn escape(input: &str) -> IResult<&str, char, Error<'_>> {
    let invalid_escape = nom::Err::Failure(Error {
        remaining: input,
        kind: ParseErrorKind::InvalidEscape,
    });

    let mut chars = input.chars();
    if chars.next() != Some('\\') {
        return Err(invalid_escape);
    }

    let simple = match chars.next() {
        Some('"') => Some('"'),
        Some('/') => Some('/'),
        Some('\\') => Some('\\'),
        Some('b') => Some('\u{8}'),
        Some('f') => Some('\u{c}'),
        Some('n') => Some('\n'),
        Some('r') => Some('\r'),
        Some('t') => Some('\t'),
        Some('u') => None,
        _ => return Err(invalid_escape),
    };
    if let Some(c) = simple {
        return Ok((chars.as_str(), c));
    }

    // `\u{...}` form.
    if let Ok((rest, digits)) =
        delimited(tag("{"), hex_digit1::<_, Error>, tag("}")).parse(chars.as_str())
    {
        return match u32::from_str_radix(digits, 16)
            .ok()
            .and_then(char::from_u32)
        {
            Some(c) => Ok((rest, c)),
            None => Err(invalid_escape),
        };
    }

    // `\uXXXX` form, potentially followed with a second `\uXXXX` in case of surrogate pairs.
    let rest = chars.as_str();
    let Some(first) = rest
        .get(..4)
        .filter(|d| d.chars().all(|c| c.is_ascii_hexdigit()))
        .and_then(|d| u32::from_str_radix(d, 16).ok())
    else {
        return Err(invalid_escape);
    };
    let rest = &rest[4..];

    if !(0xd800..0xdc00).contains(&first) {
        return match char::from_u32(first) {
            Some(c) => Ok((rest, c)),
            None => Err(invalid_escape),
        };
    }

    let Some(second) = rest
        .strip_prefix("\\u")
        .and_then(|r| r.get(..4))
        .filter(|d| d.chars().all(|c| c.is_ascii_hexdigit()))
        .and_then(|d| u32::from_str_radix(d, 16).ok())
        .filter(|second| (0xdc00..0xe000).contains(second))
    else {
        return Err(invalid_escape);
    };

    match char::from_u32(0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)) {
        Some(c) => Ok((&rest[6..], c)),
        None => Err(invalid_escape),
    }
}
