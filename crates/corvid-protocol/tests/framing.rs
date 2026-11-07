use corvid_protocol::wire::RenderHeader;
use corvid_protocol::{
    CONTROL_LIMIT, FRAME_PREAMBLE, Frame, FrameError, FrameReader, FrameRole, FrameWriter,
    RENDER_LIMIT, RenderFrame, decode_envelope, hello,
};
use prost::Message;
use prost::encoding::{WireType, encode_key, encode_varint};
use tokio::io::{AsyncWriteExt, duplex};

fn framed(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
    frame.push(kind);
    frame.extend_from_slice(payload);
    frame
}

fn render() -> RenderFrame {
    let ansi = b"\x1b[H\x1b[2J".to_vec();
    RenderFrame {
        header: RenderHeader {
            host_id: "host".into(),
            server_epoch: "epoch".into(),
            client_id: "client".into(),
            view_revision: 2,
            render_generation: 1,
            frame_id: 1,
            base_frame_id: None,
            ansi_bytes: ansi.len() as u32,
        },
        ansi,
    }
}

#[tokio::test]
async fn fragmented_preamble_and_frames_preserve_wire_boundaries() {
    let (mut tx, rx) = duplex(16);
    let payload = hello("hello-1".into(), None, vec![]).encode_to_vec();
    let mut burst = framed(1, &payload);
    burst.extend(framed(1, &payload));
    let writer = tokio::spawn(async move {
        for byte in FRAME_PREAMBLE {
            tx.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
        for chunk in burst.chunks(3) {
            tx.write_all(chunk).await.unwrap();
        }
    });

    let mut reader = FrameReader::start(rx, FrameRole::StructuredOnly)
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            reader.next_frame().await.unwrap(),
            Some(Frame::Structured(payload.clone()))
        );
    }
    assert_eq!(reader.next_frame().await.unwrap(), None);
    writer.await.unwrap();
}

#[tokio::test]
async fn rejects_bad_preamble_and_frame_lengths_before_reading_payload() {
    assert!(matches!(
        FrameReader::start(&b"CRV2"[..], FrameRole::StructuredOnly).await,
        Err(FrameError::ProtocolMismatch)
    ));
    assert!(matches!(
        FrameReader::start(&b"CR"[..], FrameRole::StructuredOnly).await,
        Err(FrameError::Truncated)
    ));

    let cases = [
        (0, 1, FrameRole::StructuredOnly, FrameError::EmptyFrame),
        (u32::MAX, 1, FrameRole::StructuredOnly, FrameError::TooLarge),
        (
            CONTROL_LIMIT as u32 + 1,
            1,
            FrameRole::TuiOutput,
            FrameError::TooLarge,
        ),
        (
            RENDER_LIMIT as u32 + 1,
            2,
            FrameRole::TuiOutput,
            FrameError::TooLarge,
        ),
        (3, 2, FrameRole::StructuredOnly, FrameError::WrongRole),
        (1, 3, FrameRole::StructuredOnly, FrameError::UnknownKind(3)),
        (3, 1, FrameRole::StructuredOnly, FrameError::Truncated),
    ];
    for (length, kind, role, expected) in cases {
        let mut bytes = FRAME_PREAMBLE.to_vec();
        bytes.extend(length.to_be_bytes());
        bytes.push(kind);
        let mut reader = FrameReader::start(bytes.as_slice(), role).await.unwrap();
        let error = reader.next_frame().await.unwrap_err();
        assert_eq!(error.to_string(), expected.to_string());
    }
}

#[tokio::test]
async fn validates_render_length_and_does_not_release_partial_ansi() {
    let frame = render();
    let mut bytes = Vec::new();
    let mut writer = FrameWriter::start(&mut bytes, FrameRole::TuiOutput)
        .await
        .unwrap();
    writer.render(&frame).await.unwrap();

    let mut reader = FrameReader::start(bytes.as_slice(), FrameRole::TuiOutput)
        .await
        .unwrap();
    assert_eq!(
        reader.next_frame().await.unwrap(),
        Some(Frame::Render(frame))
    );

    let mut reader = FrameReader::start(&bytes[..bytes.len() - 1], FrameRole::TuiOutput)
        .await
        .unwrap();
    assert!(matches!(
        reader.next_frame().await,
        Err(FrameError::Truncated)
    ));

    let mut mismatch = render();
    mismatch.header.ansi_bytes -= 1;
    let header = mismatch.header.encode_to_vec();
    let mut payload = (header.len() as u32).to_be_bytes().to_vec();
    payload.extend(header);
    payload.extend(mismatch.ansi);
    let mut bytes = FRAME_PREAMBLE.to_vec();
    bytes.extend(framed(2, &payload));
    let mut reader = FrameReader::start(bytes.as_slice(), FrameRole::TuiOutput)
        .await
        .unwrap();
    assert!(matches!(
        reader.next_frame().await,
        Err(FrameError::RenderHeader)
    ));
}

#[tokio::test]
async fn relays_unknown_fields_without_decode_or_reencode() {
    let request = hello("hello-1".into(), None, vec![]);
    let mut payload = request.encode_to_vec();
    encode_key(400, WireType::LengthDelimited, &mut payload);
    encode_varint(3, &mut payload);
    payload.extend([0xff, 0, 0x7f]);
    assert_eq!(decode_envelope(&payload).unwrap(), request);

    let mut bytes = Vec::new();
    let mut writer = FrameWriter::start(&mut bytes, FrameRole::StructuredOnly)
        .await
        .unwrap();
    writer.opaque_envelope(&payload).await.unwrap();
    let mut reader = FrameReader::start(bytes.as_slice(), FrameRole::StructuredOnly)
        .await
        .unwrap();
    assert_eq!(
        reader.next_frame().await.unwrap(),
        Some(Frame::Structured(payload))
    );
}

#[test]
fn rejects_nested_groups_bad_utf8_and_oversized_packed_collections() {
    let mut payload = Vec::new();
    for _ in 0..33 {
        encode_key(400, WireType::StartGroup, &mut payload);
    }
    for _ in 0..33 {
        encode_key(400, WireType::EndGroup, &mut payload);
    }
    assert!(matches!(
        decode_envelope(&payload),
        Err(FrameError::MalformedField)
    ));

    // Hello contains a controller_id with an invalid UTF-8 byte.
    assert!(matches!(
        decode_envelope(&[0x52, 3, 0x0a, 1, 0xff]),
        Err(FrameError::Protobuf(_))
    ));

    let mut request = hello("hello-1".into(), None, vec![]);
    let Some(corvid_protocol::wire::envelope::Body::Hello(hello)) = request.body.as_mut() else {
        unreachable!()
    };
    hello.supported_major_versions = vec![1; 1025];
    assert!(matches!(
        decode_envelope(&request.encode_to_vec()),
        Err(FrameError::CollectionLimit)
    ));
}
