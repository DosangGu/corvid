#![cfg(windows)]

use std::future::pending;
use std::time::Duration;

use corvid_companion::{HostEndpoint, PresentationClient, Store, initialize, run};
use corvid_protocol::wire::envelope::Body;
use corvid_protocol::wire::{GetPresentationSlots, Ping};
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

#[tokio::test]
async fn serves_current_user_clients_and_refuses_a_second_pipe_owner() {
    let directory = tempfile::tempdir().unwrap();
    let instance = Uuid::new_v4().simple().to_string();
    let state = directory.path().join("first");
    let duplicate_state = directory.path().join("second");
    let endpoint = HostEndpoint::Ssh {
        alias: "corvid-fixture.invalid".to_owned(),
    };
    let host = Uuid::new_v4();
    let controller = initialize(&state, host, endpoint.clone(), 2).unwrap();
    initialize(&duplicate_state, host, endpoint, 2).unwrap();
    let (stop, shutdown) = oneshot::channel();

    let mut companion = tokio::spawn({
        let instance = instance.clone();
        async move {
            run(&instance, &state, async {
                let _ = shutdown.await;
            })
            .await
        }
    });
    let mut first = timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = &mut companion => panic!("companion exited during setup: {result:?}"),
                result = PresentationClient::connect(&instance) => {
                    if let Ok(client) = result {
                        return client;
                    }
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let mut second = PresentationClient::connect(&instance).await.unwrap();
    let duplicate = timeout(
        Duration::from_secs(1),
        run(&instance, &duplicate_state, pending()),
    )
    .await
    .unwrap();
    assert!(duplicate.is_err());
    assert_eq!(first.session().host_id, host.to_string());

    let reply = first
        .request(Body::GetPresentationSlots(GetPresentationSlots::default()))
        .await
        .unwrap();
    let Body::PresentationSlots(slots) = reply else {
        panic!("missing slot snapshot")
    };
    assert_eq!(slots.controller_id, controller.to_string());
    assert_eq!(slots.slots.len(), 2);
    assert!(matches!(
        second.request(Body::Ping(Ping {})).await.unwrap(),
        Body::Pong(_)
    ));
    stop.send(()).unwrap();
    companion.await.unwrap().unwrap();
}

#[test]
fn requires_a_private_created_directory_for_state() {
    let ordinary = tempfile::tempdir().unwrap();
    let endpoint = HostEndpoint::Ssh {
        alias: "corvid-fixture.invalid".to_owned(),
    };
    let result = initialize(ordinary.path(), Uuid::new_v4(), endpoint, 1);
    assert!(result.is_err());
    assert!(Store::open(ordinary.path()).is_err());
    assert!(!ordinary.path().join("config.json").exists());
}
