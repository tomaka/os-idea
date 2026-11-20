interface_bindings::interface_bindings!("tests/request-response.cddl");
interface_bindings::interface_bindings!("tests/multiple-messages.cddl");
interface_bindings::interface_bindings!("tests/decoding.cddl");

#[test]
fn request_response_types() {
    let request = request_response::StoreRequest {
        hash: [0; 32],
        data: b"hello",
        name: Some("foo"),
        links: request_response::Links(std::collections::BTreeMap::from([([1; 32], 5)])),
        priority: 5,
        kind: request_response::Kind::Large(request_response::Large { size: 1, chunks: 2 }),
        tags: vec!["a", "b"],
    };
    assert_eq!(request.clone(), request);

    let _ = request_response::Kind::Small(request_response::Small(3));
    let _ = request_response::StoreResponse {};
}

#[test]
fn request_response_emit_signature() {
    let _: fn(&request_response::StoreRequest<'_>) -> request_response::StoreResponse =
        request_response::emit_store_request;
}

#[test]
#[should_panic]
fn emit_is_unimplemented() {
    let _ = multiple_messages::emit_close_request(&multiple_messages::CloseRequest(3));
}

#[test]
fn multiple_messages_types() {
    let _ = multiple_messages::ReadRequest {
        handle: 1,
        length: 2,
    };
    let _ = multiple_messages::ReadResponse(b"data");
    let _ = multiple_messages::WriteNotification {
        handle: 1,
        data: b"data",
    };
    let _ = multiple_messages::CloseResponse(());
    let _ = multiple_messages::PingRequest {};
    let _ = multiple_messages::PingResponse("hello", 1, 2);
    let _ = multiple_messages::ShutdownNotification(());
}

#[test]
fn multiple_messages_emit_signatures() {
    let _: fn(&multiple_messages::ReadRequest) -> multiple_messages::ReadResponse<'static> =
        multiple_messages::emit_read_request;
    let _: fn(&multiple_messages::WriteNotification<'_>) =
        multiple_messages::emit_write_notification;
    let _: fn(&multiple_messages::CloseRequest) -> multiple_messages::CloseResponse =
        multiple_messages::emit_close_request;
    let _: fn(&multiple_messages::PingRequest) -> multiple_messages::PingResponse<'static> =
        multiple_messages::emit_ping_request;
    let _: fn(&multiple_messages::ShutdownNotification) =
        multiple_messages::emit_shutdown_notification;
}

#[test]
fn decode_map_struct() {
    let parts: [&[u8]; 5] = [
        b"\xa7\x64hash\x58\x20",
        &[7; 32],
        b"\x64data\x45hello\x64name\x63foo\x65links\xa1\x58\x20",
        &[1; 32],
        b"\x05\x68priority\x0a\x64kind\x82\x01\x02\x64tags\x82\x61a\x61b",
    ];
    let data = parts.concat();
    assert_eq!(
        request_response::StoreRequest::decode(&data),
        Ok(request_response::StoreRequest {
            hash: [7; 32],
            data: b"hello",
            name: Some("foo"),
            links: request_response::Links(std::collections::BTreeMap::from([([1; 32], 5)])),
            priority: 10,
            kind: request_response::Kind::Large(request_response::Large { size: 1, chunks: 2 }),
            tags: vec!["a", "b"],
        })
    );

    let mut trailing = data.clone();
    trailing.push(0);
    assert_eq!(
        request_response::StoreRequest::decode(&trailing),
        Err(request_response::DecodeError::TrailingData)
    );

    assert_eq!(
        request_response::StoreRequest::decode(&data[..data.len() - 1]),
        Err(request_response::DecodeError::UnexpectedEnd)
    );
}

#[test]
fn decode_map_struct_optional_missing() {
    let parts: [&[u8]; 5] = [
        b"\xa6\x64hash\x58\x20",
        &[7; 32],
        b"\x64data\x45hello\x65links\xa1\x58\x20",
        &[1; 32],
        b"\x05\x68priority\x00\x64kind\x81\x03\x64tags\x80",
    ];
    let data = parts.concat();
    let request = request_response::StoreRequest::decode(&data).unwrap();
    assert_eq!(request.name, None);
    assert_eq!(request.priority, 0);
    assert_eq!(
        request.kind,
        request_response::Kind::Small(request_response::Small(3))
    );
    assert!(request.tags.is_empty());
}

#[test]
fn decode_map_struct_errors() {
    // Priority out of range.
    let parts: [&[u8]; 5] = [
        b"\xa6\x64hash\x58\x20",
        &[7; 32],
        b"\x64data\x45hello\x65links\xa1\x58\x20",
        &[1; 32],
        b"\x05\x68priority\x0b\x64kind\x81\x03\x64tags\x80",
    ];
    assert_eq!(
        request_response::StoreRequest::decode(&parts.concat()),
        Err(request_response::DecodeError::InvalidValue)
    );

    // Missing `data`.
    let parts: [&[u8]; 5] = [
        b"\xa5\x64hash\x58\x20",
        &[7; 32],
        b"\x65links\xa1\x58\x20",
        &[1; 32],
        b"\x05\x68priority\x00\x64kind\x81\x03\x64tags\x80",
    ];
    assert_eq!(
        request_response::StoreRequest::decode(&parts.concat()),
        Err(request_response::DecodeError::MissingKey)
    );

    // Unknown key.
    let parts: [&[u8]; 5] = [
        b"\xa7\x64hash\x58\x20",
        &[7; 32],
        b"\x64data\x45hello\x65links\xa1\x58\x20",
        &[1; 32],
        b"\x05\x68priority\x00\x64kind\x81\x03\x64tags\x80\x65extra\x01",
    ];
    assert_eq!(
        request_response::StoreRequest::decode(&parts.concat()),
        Err(request_response::DecodeError::UnknownKey)
    );

    // Duplicate key.
    let parts: [&[u8]; 5] = [
        b"\xa7\x64hash\x58\x20",
        &[7; 32],
        b"\x64data\x45hello\x65links\xa1\x58\x20",
        &[1; 32],
        b"\x05\x68priority\x00\x64kind\x81\x03\x64tags\x80\x64data\x40",
    ];
    assert_eq!(
        request_response::StoreRequest::decode(&parts.concat()),
        Err(request_response::DecodeError::DuplicateKey)
    );

    // Hash of the wrong size.
    let parts: [&[u8]; 5] = [
        b"\xa6\x64hash\x58\x1f",
        &[7; 31],
        b"\x64data\x45hello\x65links\xa1\x58\x20",
        &[1; 32],
        b"\x05\x68priority\x00\x64kind\x81\x03\x64tags\x80",
    ];
    assert_eq!(
        request_response::StoreRequest::decode(&parts.concat()),
        Err(request_response::DecodeError::InvalidValue)
    );
}

#[test]
fn decode_multiple_messages() {
    assert_eq!(
        multiple_messages::ReadRequest::decode(b"\xa2\x66length\x04\x66handle\x03"),
        Ok(multiple_messages::ReadRequest {
            handle: 3,
            length: 4
        })
    );
    assert_eq!(
        multiple_messages::ReadResponse::decode(b"\x44data"),
        Ok(multiple_messages::ReadResponse(b"data"))
    );
    assert_eq!(
        multiple_messages::ReadResponse::decode_partial(b"\x44data\x00\x01"),
        Ok((
            multiple_messages::ReadResponse(b"data"),
            core::num::NonZero::new(5).unwrap_or_else(|| unreachable!())
        ))
    );
    assert_eq!(
        multiple_messages::ReadResponse::decode(b"\x44data\x00"),
        Err(multiple_messages::DecodeError::TrailingData)
    );
    assert_eq!(
        multiple_messages::CloseResponse::decode(b"\xf6"),
        Ok(multiple_messages::CloseResponse(()))
    );
    assert_eq!(
        multiple_messages::PingRequest::decode(b"\x80"),
        Ok(multiple_messages::PingRequest {})
    );
    assert_eq!(
        multiple_messages::PingResponse::decode(b"\x83\x65hello\x01\x02"),
        Ok(multiple_messages::PingResponse("hello", 1, 2))
    );
    assert_eq!(
        multiple_messages::PingResponse::decode(b"\x82\x65hello\x01"),
        Err(multiple_messages::DecodeError::WrongItemCount)
    );
    assert_eq!(
        multiple_messages::PingResponse::decode(b"\x84\x65hello\x01\x02\x03"),
        Err(multiple_messages::DecodeError::WrongItemCount)
    );
    assert_eq!(
        multiple_messages::CloseRequest::decode(b"\x9f\xff"),
        Err(multiple_messages::DecodeError::IndefiniteLength)
    );
    assert_eq!(
        multiple_messages::CloseRequest::decode(b"\x20"),
        Err(multiple_messages::DecodeError::UnexpectedType)
    );
}

#[test]
fn decode_values() {
    assert_eq!(
        decoding::ValuesRequest::decode(
            b"\x8a\xfb\x3f\xf8\x00\x00\x00\x00\x00\x00\xf9\xc0\x00\xf4\xf5\x22\x28\x24\x07\x61x\x61y"
        ),
        Ok(decoding::ValuesRequest(
            1.5,
            -2.0,
            false,
            true,
            -3,
            -9,
            -5,
            Some(7),
            vec!["x", "y"]
        ))
    );
    assert_eq!(
        decoding::ValuesRequest::decode(
            b"\x87\xf9\x38\x00\xfa\x3f\xa0\x00\x00\xf5\xf5\x20\x04\x04"
        ),
        Ok(decoding::ValuesRequest(
            0.5,
            1.25,
            true,
            true,
            -1,
            4,
            4,
            None,
            vec![]
        ))
    );
    assert_eq!(
        decoding::ValuesRequest::decode(
            b"\x88\xf9\x38\x00\xfa\x3f\xa0\x00\x00\xf5\xf5\x20\x04\x04\x61z"
        ),
        Ok(decoding::ValuesRequest(
            0.5,
            1.25,
            true,
            true,
            -1,
            4,
            4,
            None,
            vec!["z"]
        ))
    );
    assert_eq!(
        decoding::ValuesRequest::decode(
            b"\x87\xf9\x38\x00\xfa\x3f\xa0\x00\x00\xf5\xf4\x20\x04\x04"
        ),
        Err(decoding::DecodeError::InvalidValue)
    );
    assert_eq!(
        decoding::ValuesRequest::decode(
            b"\x87\xf9\x38\x00\xfa\x3f\xa0\x00\x00\xf5\xf5\x01\x04\x04"
        ),
        Err(decoding::DecodeError::UnexpectedType)
    );
    assert_eq!(
        decoding::ValuesRequest::decode(
            b"\x87\xf9\x38\x00\xfa\x3f\xa0\x00\x00\xf5\xf5\x20\x04\x06"
        ),
        Err(decoding::DecodeError::InvalidValue)
    );
}

#[test]
fn decode_choices() {
    assert_eq!(
        decoding::ValuesResponse::decode(b"\x05"),
        Ok(decoding::ValuesResponse::Uint(5))
    );
    assert_eq!(
        decoding::ValuesResponse::decode(b"\x62hi"),
        Ok(decoding::ValuesResponse::Tstr("hi"))
    );
    assert_eq!(
        decoding::ValuesResponse::decode(b"\x82\xf5\xf4"),
        Ok(decoding::ValuesResponse::Variant2(vec![true, false]))
    );
    assert_eq!(
        decoding::ValuesResponse::decode(b"\x81\xf5"),
        Err(decoding::DecodeError::NoMatchingChoice)
    );
    assert_eq!(
        decoding::ValuesResponse::decode(b"\xf5"),
        Err(decoding::DecodeError::NoMatchingChoice)
    );
}

#[test]
fn encode_map_struct() {
    let parts: [&[u8]; 5] = [
        b"\xa7\x64hash\x58\x20",
        &[7; 32],
        b"\x64data\x45hello\x64name\x63foo\x65links\xa1\x58\x20",
        &[1; 32],
        b"\x05\x68priority\x0a\x64kind\x82\x01\x02\x64tags\x82\x61a\x61b",
    ];
    let data = parts.concat();
    let request = request_response::StoreRequest::decode(&data).unwrap();
    let mut encoded = Vec::new();
    request.encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(encoded, data);

    let request = request_response::StoreRequest {
        hash: [0; 32],
        data: b"",
        name: None,
        links: request_response::Links(std::collections::BTreeMap::new()),
        priority: 0,
        kind: request_response::Kind::Small(request_response::Small(70000)),
        tags: vec![],
    };
    let mut encoded = Vec::new();
    request
        .clone()
        .encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(
        request_response::StoreRequest::decode(&encoded),
        Ok(request)
    );
}

#[test]
fn encode_multiple_messages() {
    let mut encoded = Vec::new();
    multiple_messages::ReadRequest {
        handle: 3,
        length: 300,
    }
    .encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(encoded, b"\xa2\x66handle\x03\x66length\x19\x01\x2c");

    let mut encoded = Vec::new();
    multiple_messages::PingResponse("hello", 1, 2)
        .encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(encoded, b"\x83\x65hello\x01\x02");

    let mut encoded = Vec::new();
    multiple_messages::CloseResponse(()).encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(encoded, b"\xf6");

    let mut encoded = Vec::new();
    multiple_messages::PingRequest {}.encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(encoded, b"\x80");
}

#[test]
fn encode_values() {
    let request = decoding::ValuesRequest(
        1.5,
        -2.0,
        false,
        true,
        -3,
        i64::MIN,
        -5,
        Some(7),
        vec!["x", "y"],
    );
    let mut encoded = Vec::new();
    request
        .clone()
        .encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(
        encoded,
        b"\x8a\xfb\x3f\xf8\x00\x00\x00\x00\x00\x00\xfa\xc0\x00\x00\x00\xf4\xf5\x22\
        \x3b\x7f\xff\xff\xff\xff\xff\xff\xff\x24\x07\x61x\x61y"
    );
    assert_eq!(decoding::ValuesRequest::decode(&encoded), Ok(request));

    let request = decoding::ValuesRequest(0.5, 1.25, true, true, -1, 4, 4, None, vec!["z"]);
    let mut encoded = Vec::new();
    request
        .clone()
        .encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(decoding::ValuesRequest::decode(&encoded), Ok(request));

    let mut encoded = Vec::new();
    decoding::ValuesResponse::Variant2(vec![true, false, true])
        .encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(encoded, b"\x83\xf5\xf4\xf5");

    let mut encoded = Vec::new();
    decoding::ValuesResponse::Tstr("hi").encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(encoded, b"\x62hi");
}

#[test]
fn implementation_names_are_free() {
    let mut encoded = Vec::new();
    decoding::Encoder(decoding::Decoder(5)).encode_into(|bytes| encoded.extend_from_slice(bytes));
    assert_eq!(encoded, b"\x81\x05");
    assert_eq!(
        decoding::Encoder::decode(&encoded),
        Ok(decoding::Encoder(decoding::Decoder(5)))
    );
}
