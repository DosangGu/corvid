use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{Attach, AttachRole, InputChunk, Ping};
use corvid_protocol::{PeerRole, accept_hello, check_hello_result, decode_envelope, hello};
use prost::Message;
use prost::encoding::{WireType, encode_key, encode_varint};

#[test]
fn newer_client_negotiates_supported_version_and_capabilities() {
    let mut request = hello(
        "req".into(),
        Some("controller".into()),
        vec!["ping.v1".into()],
    );
    request.protocol_minor = 8;
    let (server, reply) = accept_hello(
        &request,
        PeerRole::Control,
        "host",
        "epoch",
        &["ping.v1", "host.v1"],
    )
    .unwrap();
    assert_eq!(reply.protocol_minor, 0);
    let client = check_hello_result(&request, &reply, PeerRole::Control).unwrap();
    assert_eq!(client, server);

    let mut ping = client.envelope(Some("ping".into()), Body::Ping(Ping {}));
    assert!(server.validate_request(&ping).is_ok());
    ping.server_epoch = Some("old-epoch".into());
    assert_eq!(
        server.validate_request(&ping).unwrap_err().code,
        "EpochMismatch"
    );
}

#[test]
fn reports_missing_capability_and_version_without_granting_a_session() {
    let mut request = hello("req".into(), None, vec!["render.v1".into()]);
    let error = accept_hello(&request, PeerRole::Tui, "host", "epoch", &["ping.v1"]).unwrap_err();
    assert_eq!(error.code, "UnsupportedCapability");

    let Some(Body::Hello(body)) = &mut request.body else {
        unreachable!()
    };
    request.protocol_major = 2;
    body.supported_major_versions = vec![2];
    let error = accept_hello(&request, PeerRole::Tui, "host", "epoch", &["render.v1"]).unwrap_err();
    assert_eq!(error.code, "ProtocolMismatch");
    assert_eq!(error.supported_major_versions, [1]);
}

#[test]
fn unknown_body_and_attach_enum_cannot_select_a_default_action() {
    let request = hello("hello".into(), None, vec![]);
    let (session, _) = accept_hello(&request, PeerRole::Tui, "host", "epoch", &[]).unwrap();
    let mut attach = session.envelope(
        Some("attach".into()),
        Body::Attach(Attach {
            requested_role: 2000,
            ..Default::default()
        }),
    );
    assert_eq!(
        session.validate_request(&attach).unwrap_err().code,
        "UnsupportedCapability"
    );

    attach.body = None;
    let mut encoded = attach.encode_to_vec();
    encode_key(120, WireType::LengthDelimited, &mut encoded);
    encode_varint(0, &mut encoded);
    let decoded = decode_envelope(&encoded).unwrap();
    assert_eq!(
        session.validate_request(&decoded).unwrap_err().code,
        "UnsupportedCapability"
    );
}

#[test]
fn control_session_cannot_attach_or_submit_input() {
    let request = hello("hello".into(), Some("controller".into()), vec![]);
    let (session, _) = accept_hello(&request, PeerRole::Control, "host", "epoch", &[]).unwrap();
    let attach = session.envelope(
        Some("attach".into()),
        Body::Attach(Attach {
            requested_role: AttachRole::Writer as i32,
            ..Default::default()
        }),
    );
    assert_eq!(
        session.validate_request(&attach).unwrap_err().code,
        "UnsupportedCapability"
    );

    let input = session.envelope(
        Some("input".into()),
        Body::InputChunk(InputChunk {
            data: b"hello".to_vec(),
            ..Default::default()
        }),
    );
    assert_eq!(
        session.validate_request(&input).unwrap_err().code,
        "UnsupportedCapability"
    );
}
