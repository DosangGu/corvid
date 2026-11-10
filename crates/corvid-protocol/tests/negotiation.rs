use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{
    Attach, AttachRole, CollectHook, FocusAgentRequest, FocusSlot, GetCapabilities,
    GetPresentationSlots, InputChunk, ListHosts, Ping, ProviderAdapter, ProviderExecutionMode,
    ProviderProfile, UpgradeDaemon,
};
use corvid_protocol::{
    PeerRole, accept_hello, accept_hello_with_profiles, check_hello_result, decode_envelope, hello,
};
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
    assert_eq!(client.protocol_minor, reply.protocol_minor);

    let mut ping = client.envelope(Some("ping".into()), Body::Ping(Ping {}));
    assert!(server.validate_request(&ping).is_ok());
    ping.protocol_minor = request.protocol_minor;
    assert_eq!(
        server.validate_request(&ping).unwrap_err().code,
        "ProtocolMismatch"
    );
    ping.protocol_minor = client.protocol_minor;
    ping.server_epoch = Some("old-epoch".into());
    assert_eq!(
        server.validate_request(&ping).unwrap_err().code,
        "EpochMismatch"
    );
}

#[test]
fn a_hello_result_above_the_offered_minor_is_rejected() {
    let request = hello("hello".into(), None, vec![]);
    let (_, mut reply) = accept_hello(&request, PeerRole::Tui, "host", "epoch", &[]).unwrap();
    reply.protocol_minor = request.protocol_minor + 1;
    let fault = check_hello_result(&request, &reply, PeerRole::Tui).unwrap_err();
    assert_eq!(fault.code, "ProtocolMismatch");
}

#[test]
fn hello_preserves_an_additive_profile_and_rejects_duplicate_profile_identity() {
    let request = hello("hello".into(), Some("controller".into()), vec![]);
    let profile = ProviderProfile {
        provider: 101,
        version: "1.2.3".into(),
        adapter: Some(ProviderAdapter {
            mode: ProviderExecutionMode::InteractiveCli as i32,
            schema_revision: 1,
            ..Default::default()
        }),
    };
    let (_, mut reply) = accept_hello_with_profiles(
        &request,
        PeerRole::Control,
        "host",
        "epoch",
        &[],
        std::slice::from_ref(&profile),
    )
    .unwrap();
    let session = check_hello_result(&request, &reply, PeerRole::Control).unwrap();
    assert_eq!(
        session.provider_profiles.as_slice(),
        std::slice::from_ref(&profile)
    );

    let Some(Body::HelloResult(description)) = &mut reply.body else {
        unreachable!()
    };
    description.provider_profiles.push(profile);
    assert_eq!(
        check_hello_result(&request, &reply, PeerRole::Control)
            .unwrap_err()
            .code,
        "InvalidMessage"
    );
}

#[test]
fn only_the_control_role_can_query_capabilities() {
    let request = hello("hello".into(), Some("controller".into()), vec![]);
    for role in [
        PeerRole::Control,
        PeerRole::Tui,
        PeerRole::Presentation,
        PeerRole::Collector,
    ] {
        let (session, _) = accept_hello(&request, role, "host", "epoch", &[]).unwrap();
        let request = session.envelope(
            Some("query".into()),
            Body::GetCapabilities(GetCapabilities {}),
        );
        assert_eq!(
            session.validate_request(&request).is_ok(),
            role == PeerRole::Control
        );
    }
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

#[test]
fn only_the_local_upgrade_role_accepts_daemon_upgrade() {
    let request = hello("hello".into(), Some("controller".into()), vec![]);
    for role in [
        PeerRole::Upgrade,
        PeerRole::Control,
        PeerRole::Tui,
        PeerRole::Collector,
        PeerRole::Presentation,
    ] {
        let (session, _) = accept_hello(&request, role, "host", "epoch", &[]).unwrap();
        let request = session.envelope(
            Some("upgrade".into()),
            Body::UpgradeDaemon(UpgradeDaemon {}),
        );
        assert_eq!(
            session.validate_request(&request).is_ok(),
            role == PeerRole::Upgrade
        );
    }
}

#[test]
fn collector_is_separate_from_control_and_terminal_roles() {
    let request = hello("hello".into(), None, vec![]);
    let (session, _) = accept_hello(&request, PeerRole::Collector, "host", "epoch", &[]).unwrap();
    let mut hook = session.envelope(
        Some("delivery".into()),
        Body::CollectHook(CollectHook::default()),
    );
    assert!(session.validate_request(&hook).is_ok());
    hook.server_epoch = Some("old-epoch".into());
    assert_eq!(
        session.validate_request(&hook).unwrap_err().code,
        "EpochMismatch"
    );

    let input = session.envelope(
        Some("input".into()),
        Body::InputChunk(InputChunk::default()),
    );
    assert_eq!(
        session.validate_request(&input).unwrap_err().code,
        "UnsupportedCapability"
    );

    let request = hello("hello".into(), Some("controller".into()), vec![]);
    for role in [PeerRole::Control, PeerRole::Tui] {
        let (session, _) = accept_hello(&request, role, "host", "epoch", &[]).unwrap();
        let hook = session.envelope(
            Some("delivery".into()),
            Body::CollectHook(CollectHook::default()),
        );
        assert_eq!(
            session.validate_request(&hook).unwrap_err().code,
            "UnsupportedCapability"
        );
    }
}

#[test]
fn presentation_accepts_host_and_slot_requests_without_remote_control_or_terminal_input() {
    let request = hello("hello".into(), None, vec![]);
    let (session, _) =
        accept_hello(&request, PeerRole::Presentation, "host", "local-epoch", &[]).unwrap();
    for body in [
        Body::ListHosts(ListHosts {}),
        Body::GetPresentationSlots(GetPresentationSlots::default()),
        Body::FocusSlot(FocusSlot::default()),
    ] {
        let request = session.envelope(Some("presentation".into()), body);
        assert!(session.validate_request(&request).is_ok());
    }
    for body in [
        Body::FocusAgent(FocusAgentRequest::default()),
        Body::InputChunk(InputChunk::default()),
    ] {
        let request = session.envelope(Some("presentation".into()), body);
        assert_eq!(
            session.validate_request(&request).unwrap_err().code,
            "UnsupportedCapability"
        );
    }

    let request = hello("hello".into(), Some("controller".into()), vec![]);
    let (session, _) =
        accept_hello(&request, PeerRole::Control, "host", "remote-epoch", &[]).unwrap();
    for body in [
        Body::ListHosts(ListHosts {}),
        Body::FocusSlot(FocusSlot::default()),
    ] {
        let request = session.envelope(Some("presentation".into()), body);
        assert_eq!(
            session.validate_request(&request).unwrap_err().code,
            "UnsupportedCapability"
        );
    }
}
