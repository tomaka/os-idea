interface_bindings::interface_bindings!("tests/request-response.cddl");
interface_bindings::interface_bindings!("tests/multiple-messages.cddl");

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
