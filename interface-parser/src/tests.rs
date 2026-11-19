#![cfg(test)]

use crate::{
    Assignment, GroupEntry, GroupEntryKind, HeadNumber, MemberKey, Message, Occurrence, Operator,
    ParseErrorKind, RuleBody, Type1, Type2, Value,
};

use alloc::{string::String, vec, vec::Vec};

#[test]
fn single_request_response() {
    let source = include_bytes!("tests/single-request-response.cddl");
    let Ok(definition) = crate::parse(source) else {
        panic!()
    };

    assert_eq!(
        definition
            .rules
            .iter()
            .map(|rule| rule.name.as_str())
            .collect::<Vec<_>>(),
        ["message", "spawn-request", "spawn-response", "links"]
    );

    for rule in &definition.rules {
        assert!(source[rule.span.clone()].starts_with(rule.name.as_bytes()));
    }
    assert_eq!(
        &source[definition.rules[0].span.clone()],
        b"message = [ spawn-request, spawn-response ]"
    );
    assert_eq!(
        &source[definition.rules[3].span.clone()],
        b"links = {\n    * bytes .size 32 => bytes .size 32\n}"
    );

    let [
        Message::RequestResponse {
            request,
            response,
            span,
        },
    ] = &definition.messages[..]
    else {
        panic!()
    };
    assert_eq!(&source[span.clone()], b"[ spawn-request, spawn-response ]");
    assert!(
        matches!(&request.choices[..], [Type1 { target: Type2::Name { name, .. }, .. }] if name == "spawn-request")
    );
    assert!(
        matches!(&response.choices[..], [Type1 { target: Type2::Name { name, .. }, .. }] if name == "spawn-response")
    );

    // `links` is converted from a type member into a group name only if it is a group, which
    // isn't the case here.
    let RuleBody::Type(spawn_request) = &definition.rules[1].body else {
        panic!()
    };
    let Type2::Array(group) = &spawn_request.choices[0].target else {
        panic!()
    };
    let [first, second] = &group.choices[0].entries[..] else {
        panic!()
    };
    assert!(matches!(
        &first.kind,
        GroupEntryKind::Member { key: None, ty }
            if matches!(&ty.choices[0].operator, Some((Operator::Control(op), Type2::Value(Value::Integer(32)))) if op == "size")
    ));
    assert!(matches!(
        &second.kind,
        GroupEntryKind::Member { key: None, .. }
    ));
}

#[test]
fn multiple_messages() {
    let source = include_bytes!("tests/multiple-messages.cddl");
    let Ok(definition) = crate::parse(source) else {
        panic!()
    };

    let kinds = definition
        .messages
        .iter()
        .map(|message| match message {
            Message::RequestResponse { span, .. } => (true, &source[span.clone()]),
            Message::Notification { span, .. } => (false, &source[span.clone()]),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        kinds,
        [
            (true, &b"[ read-request, read-response ]"[..]),
            (false, &b"write-notification"[..]),
            (
                true,
                &b"[ request: close-request, response: close-response ]"[..]
            ),
            (false, &b"[ uint, uint, uint ]"[..]),
            (true, &b"[ ping-request, ping-response ]"[..]),
            (false, &b"shutdown-notification"[..]),
        ]
    );

    let extension = definition
        .rules
        .iter()
        .filter(|rule| rule.name == "message")
        .map(|rule| rule.assignment)
        .collect::<Vec<_>>();
    assert_eq!(
        extension,
        [Assignment::Define, Assignment::TypeChoiceExtension]
    );
}

#[test]
fn full_syntax() {
    let source = include_bytes!("tests/full-syntax.cddl");
    let definition = match crate::parse(source) {
        Ok(d) => d,
        Err(err) => panic!("{err}"),
    };

    assert_eq!(definition.messages.len(), 2);
    assert!(matches!(
        definition.messages[0],
        Message::RequestResponse { .. }
    ));
    assert!(matches!(
        definition.messages[1],
        Message::Notification {
            ty: Type1 {
                target: Type2::Name { .. },
                ..
            },
            ..
        }
    ));

    let Some(everything) = definition.rules.iter().find(|r| r.name == "everything") else {
        panic!()
    };
    let RuleBody::Type(everything) = &everything.body else {
        panic!()
    };
    let Type2::Map(everything) = &everything.choices[0].target else {
        panic!()
    };
    let entries = &everything.choices[0].entries;
    assert_eq!(entries.len(), 12);

    let values = entries[..4]
        .iter()
        .map(|entry| {
            let GroupEntryKind::Member { ty, .. } = &entry.kind else {
                panic!()
            };
            match &ty.choices[0].target {
                Type2::Value(value) => vec![value.clone()],
                Type2::Array(array) => array.choices[0]
                    .entries
                    .iter()
                    .map(|entry| match &entry.kind {
                        GroupEntryKind::Member { ty, .. } => match &ty.choices[0].target {
                            Type2::Value(value) => value.clone(),
                            _ => panic!(),
                        },
                        _ => panic!(),
                    })
                    .collect::<Vec<_>>(),
                _ => panic!(),
            }
        })
        .collect::<Vec<_>>();

    assert_eq!(
        values,
        [
            vec![
                Value::Integer(0),
                Value::Integer(42),
                Value::Integer(-7),
                Value::Integer(0x1f),
                Value::Integer(0b101),
            ],
            vec![Value::Float(1.5), Value::Float(-2500.0), Value::Float(3.0)],
            vec![Value::Text(String::from("tab\there \u{e9} 😀 😀"))],
            vec![
                Value::Bytes(b"abc'".to_vec()),
                Value::Bytes(vec![0x00, 0xff, 0x10]),
                Value::Bytes(b"hello".to_vec()),
            ],
        ]
    );

    assert_eq!(
        entries[2].occurrence,
        Some(Occurrence {
            min: 0,
            max: Some(1)
        })
    );

    let GroupEntryKind::Member { ty: ranges, .. } = &entries[4].kind else {
        panic!()
    };
    assert!(matches!(
        ranges.choices[0].operator,
        Some((Operator::InclusiveRange, _))
    ));
    assert!(matches!(
        ranges.choices[1].operator,
        Some((Operator::ExclusiveRange, _))
    ));

    assert_eq!(
        entries[5].occurrence,
        Some(Occurrence { min: 1, max: None })
    );
    assert!(matches!(
        &entries[5].kind,
        GroupEntryKind::Member {
            key: Some(MemberKey::Type {
                ty: Type1 {
                    target: Type2::Value(Value::Text(key)),
                    ..
                },
                cut: false
            }),
            ..
        } if key == "quoted key"
    ));

    assert!(matches!(
        &entries[6].kind,
        GroupEntryKind::Member { key: Some(MemberKey::Value(Value::Integer(1))), ty }
            if matches!(&ty.choices[0].target, Type2::Unwrap { name, .. } if name == "wrapped")
    ));
    assert!(matches!(
        &entries[7].kind,
        GroupEntryKind::Member { ty, .. }
            if matches!(&ty.choices[0].target, Type2::ChoiceFromGroupName { name, .. } if name == "colors")
    ));
    assert!(matches!(
        &entries[8].kind,
        GroupEntryKind::Member { ty, .. }
            if matches!(&ty.choices[0].target, Type2::ChoiceFromGroup(_))
    ));
    assert!(matches!(
        &entries[9],
        GroupEntry { occurrence: None, kind: GroupEntryKind::GroupName { name, .. } }
            if name == "optional-pair"
    ));
    assert!(matches!(
        &entries[10],
        GroupEntry {
            occurrence: Some(Occurrence { min: 0, max: None }),
            kind: GroupEntryKind::Member {
                key: Some(MemberKey::Type { cut: true, .. }),
                ty
            }
        } if matches!(&ty.choices[0].target, Type2::Name { name, generic_arguments } if name == "generic" && generic_arguments.len() == 2)
    ));

    let GroupEntry {
        occurrence: Some(Occurrence {
            min: 2,
            max: Some(3),
        }),
        kind:
            GroupEntryKind::Member {
                key: Some(MemberKey::Bareword(key)),
                ty: tags,
            },
    } = &entries[11]
    else {
        panic!()
    };
    assert_eq!(key, "any");
    assert!(matches!(
        tags.choices[0].target,
        Type2::Tag {
            tag: Some(HeadNumber::Literal(32)),
            ..
        }
    ));
    assert!(matches!(
        tags.choices[1].target,
        Type2::Tag {
            tag: Some(HeadNumber::Type(_)),
            ..
        }
    ));
    assert!(matches!(
        tags.choices[2].target,
        Type2::MajorType {
            major: 1,
            argument: Some(HeadNumber::Literal(5))
        }
    ));
    assert!(matches!(
        tags.choices[3].target,
        Type2::MajorType {
            major: 7,
            argument: None
        }
    ));
    assert!(matches!(tags.choices[4].target, Type2::Any));

    // Rules that are aliases of groups are converted into group entries.
    let Some(alias) = definition.rules.iter().find(|r| r.name == "alias") else {
        panic!()
    };
    assert!(matches!(
        &alias.body,
        RuleBody::GroupEntry(entry)
            if matches!(&entry.kind, GroupEntryKind::GroupName { name, .. } if name == "colors")
    ));

    let Some(choices) = definition.rules.iter().find(|r| r.name == "choices") else {
        panic!()
    };
    assert!(matches!(
        &choices.body,
        RuleBody::Type(ty) if matches!(&ty.choices[0].target, Type2::Map(group) if group.choices.len() == 2)
    ));

    let assignments = definition
        .rules
        .iter()
        .filter(|r| r.name == "extended" || r.name == "extensible-group")
        .map(|r| r.assignment)
        .collect::<Vec<_>>();
    assert_eq!(
        assignments,
        [
            Assignment::Define,
            Assignment::TypeChoiceExtension,
            Assignment::Define,
            Assignment::GroupChoiceExtension
        ]
    );
}

#[test]
fn empty_document() {
    assert!(matches!(
        crate::parse(b""),
        Err(err) if *err.kind() == ParseErrorKind::EmptyDocument
    ));
    assert!(matches!(
        crate::parse(b"  ; only a comment\n"),
        Err(err) if *err.kind() == ParseErrorKind::EmptyDocument
    ));
}

#[test]
fn root_is_group() {
    let Err(err) = crate::parse(include_bytes!("tests/root-is-group.cddl")) else {
        panic!()
    };
    assert_eq!(*err.kind(), ParseErrorKind::RootNotTypeRule);
    assert_eq!(err.offset(), 0);
}

#[test]
fn undefined_name() {
    let Err(err) = crate::parse(include_bytes!("tests/undefined-name.cddl")) else {
        panic!()
    };
    assert_eq!(
        *err.kind(),
        ParseErrorKind::UndefinedName(String::from("response"))
    );
    assert_eq!(err.offset(), 0);
}

#[test]
fn duplicate_rule() {
    let Err(err) = crate::parse(include_bytes!("tests/duplicate-rule.cddl")) else {
        panic!()
    };
    assert_eq!(
        *err.kind(),
        ParseErrorKind::DuplicateRule(String::from("foo"))
    );
    assert_eq!(err.offset(), 25);
}

#[test]
fn group_used_as_type() {
    let Err(err) = crate::parse(include_bytes!("tests/group-used-as-type.cddl")) else {
        panic!()
    };
    assert_eq!(
        *err.kind(),
        ParseErrorKind::GroupUsedAsType(String::from("colors"))
    );
}

#[test]
fn wrong_generic_arity() {
    let Err(err) = crate::parse(include_bytes!("tests/wrong-generic-arity.cddl")) else {
        panic!()
    };
    assert_eq!(
        *err.kind(),
        ParseErrorKind::WrongGenericArity(String::from("generic"))
    );
}

#[test]
fn invalid_escape() {
    let Err(err) = crate::parse(include_bytes!("tests/invalid-escape.cddl")) else {
        panic!()
    };
    assert_eq!(*err.kind(), ParseErrorKind::InvalidEscape);
    assert_eq!(err.offset(), 11);
}

#[test]
fn syntax_error() {
    let Err(err) = crate::parse(include_bytes!("tests/syntax-error.cddl")) else {
        panic!()
    };
    assert_eq!(*err.kind(), ParseErrorKind::Syntax);
    // The error is reported at the end of the document, where `]` is missing.
    assert_eq!(err.offset(), 38);
}
