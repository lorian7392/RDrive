use super::*;
use futures_core::future::BoxFuture;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::sync::mpsc;

struct ScriptedTransport {
    incoming: Option<mpsc::Receiver<Vec<u8>>>,
    reads: Arc<AtomicUsize>,
    events: Arc<std::sync::Mutex<Vec<String>>>,
}

impl SmbTransportRead for ScriptedTransport {
    fn receive_exact<'a>(
        &'a mut self,
        _: &'a mut [u8],
    ) -> BoxFuture<'a, smb_transport::error::Result<()>> {
        Box::pin(async { unreachable!("the peer supplies complete SMB packets") })
    }

    fn receive<'a>(&'a mut self) -> BoxFuture<'a, smb_transport::error::Result<Vec<u8>>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let packet = self
                .incoming
                .as_mut()
                .unwrap()
                .recv()
                .await
                .ok_or(TransportError::NotConnected)?;
            let status = u32::from_le_bytes(packet[8..12].try_into().unwrap());
            let credits = u16::from_le_bytes(packet[14..16].try_into().unwrap());
            self.events
                .lock()
                .unwrap()
                .push(format!("receive status={status:#x} credits={credits}"));
            Ok(packet)
        })
    }
}

impl SmbTransportWrite for ScriptedTransport {
    fn send_raw<'a>(&'a mut self, _: &'a [u8]) -> BoxFuture<'a, smb_transport::error::Result<()>> {
        Box::pin(async { unreachable!("the peer records complete SMB packets") })
    }

    fn send<'a>(
        &'a mut self,
        message: &'a IoVec,
    ) -> BoxFuture<'a, smb_transport::error::Result<()>> {
        Box::pin(async move {
            let packet: Vec<u8> = message
                .iter()
                .flat_map(|part| part.iter().copied())
                .collect();
            let id = u64::from_le_bytes(packet[24..32].try_into().unwrap());
            self.events
                .lock()
                .unwrap()
                .push(format!("send message_id={id}"));
            Ok(())
        })
    }
}

impl SmbTransport for ScriptedTransport {
    fn connect<'a>(
        &'a mut self,
        _: &'a str,
        _: SocketAddr,
    ) -> BoxFuture<'a, smb_transport::error::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn default_port(&self) -> u16 {
        445
    }
    fn remote_address(&self) -> smb_transport::error::Result<SocketAddr> {
        Ok("127.0.0.1:445".parse().unwrap())
    }
    fn split(
        self: Box<Self>,
    ) -> smb_transport::error::Result<(Box<dyn SmbTransportRead>, Box<dyn SmbTransportWrite>)> {
        let writer = ScriptedTransport {
            incoming: None,
            reads: self.reads.clone(),
            events: self.events.clone(),
        };
        Ok((self, Box::new(writer)))
    }
}

fn async_echo(status: smb_msg::Status, credits: u16) -> Vec<u8> {
    let mut packet = vec![0; 64];
    packet[..4].copy_from_slice(b"\xfeSMB");
    packet[4..6].copy_from_slice(&64_u16.to_le_bytes());
    packet[6..8].copy_from_slice(&1_u16.to_le_bytes());
    packet[8..12].copy_from_slice(&(status as u32).to_le_bytes());
    packet[12..14].copy_from_slice(&(smb_msg::Command::Echo as u16).to_le_bytes());
    packet[14..16].copy_from_slice(&credits.to_le_bytes());
    packet[16..20].copy_from_slice(&3_u32.to_le_bytes()); // response + async
    packet[32..40].copy_from_slice(&7_u64.to_le_bytes());
    if status != smb_msg::Status::Success {
        packet.extend_from_slice(&[9, 0, 0, 0, 0, 0, 0, 0]);
    } else {
        packet.extend_from_slice(&[4, 0, 0, 0]);
    }
    packet
}

struct Fixture {
    handler: Arc<ConnectionMessageHandler>,
    worker: Arc<WorkerImpl>,
    sender: mpsc::Sender<Vec<u8>>,
    reads: Arc<AtomicUsize>,
    events: Arc<std::sync::Mutex<Vec<String>>>,
}

async fn fixture() -> Fixture {
    fixture_with_large_mtu(true).await
}

async fn fixture_with_large_mtu(large_mtu: bool) -> Fixture {
    let (sender, incoming) = mpsc::channel(2);
    let reads = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let peer = ScriptedTransport {
        incoming: Some(incoming),
        reads: reads.clone(),
        events: events.clone(),
    };
    let worker = WorkerImpl::start(Box::new(peer), Duration::from_secs(1))
        .await
        .unwrap();
    let handler = Arc::new(ConnectionMessageHandler::new(Guid::generate(), Some(1)));
    handler
        .conn_info
        .set(Arc::new(ConnectionInfo {
            server_name: "synthetic".into(),
            server_address: "127.0.0.1:445".parse().unwrap(),
            negotiation: NegotiatedProperties {
                server_guid: Guid::generate(),
                caps: smb_msg::GlobalCapabilities::new().with_large_mtu(large_mtu),
                max_transact_size: 65536,
                max_read_size: 65536,
                max_write_size: 65536,
                auth_buffer: Vec::new(),
                signing_algo: None,
                encryption_cipher: None,
                compression: None,
                dialect_rev: smb_msg::Dialect::Smb030,
            },
            dialect: DialectImpl::new(smb_msg::Dialect::Smb030),
            config: ConnectionConfig::default(),
            preauth_hash: PreauthHashState::unsupported(),
            client_guid: Guid::generate(),
        }))
        .unwrap();
    handler.worker.set(worker.clone()).unwrap();
    Fixture {
        handler,
        worker,
        sender,
        reads,
        events,
    }
}

#[tokio::test]
async fn pending_credit_grant_is_available_before_final_response_and_for_next_request() {
    let Fixture {
        handler,
        worker,
        sender,
        reads,
        events,
    } = fixture().await;
    let sent = handler
        .sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into()))
        .await
        .unwrap();
    assert_eq!(handler.curr_credits.available_permits(), 0);
    let receive = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .recvo(
                    ReceiveOptions::new()
                        .with_msg_id_filter(sent.msg_id)
                        .with_allow_async(true),
                )
                .await
        }
    });
    sender
        .send(async_echo(smb_msg::Status::Pending, 1))
        .await
        .unwrap();
    // Wait for the actual worker to consume Pending and register its final
    // response waiter; this avoids overwriting a queued packet with the same ID.
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if reads.load(Ordering::SeqCst) >= 2
                && worker.state.lock().await.unwrap().awaiting.contains_key(&0)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let pending_credits = handler.curr_credits.available_permits();
    sender
        .send(async_echo(smb_msg::Status::Success, 0))
        .await
        .unwrap();
    let response = receive.await.unwrap().unwrap();
    assert_eq!(
        response.message.header.credit_request, 0,
        "the final packet must stay unchanged"
    );
    let final_credits = handler.curr_credits.available_permits();
    let next = tokio::time::timeout(
        Duration::from_millis(50),
        handler.sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into())),
    )
    .await;
    if matches!(next, Ok(Ok(_))) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event == "send message_id=1")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    worker.stop().await.unwrap();
    eprintln!(
        "peer events: {:?}; credits after Pending={pending_credits}, final={final_credits}; next={next:?}",
        events.lock().unwrap()
    );
    assert_eq!(
        pending_credits, 1,
        "the interim grant must be usable while the final response is pending"
    );
    assert_eq!(
        final_credits, 1,
        "the zero-credit final response must not erase the interim grant"
    );
    assert_eq!(
        handler.credit_pool.load(Ordering::SeqCst),
        1,
        "the original request must be charged exactly once"
    );
    assert!(
        matches!(next, Ok(Ok(_))),
        "the next request must not wait forever for a lost credit"
    );
}

#[tokio::test]
async fn malformed_pending_responses_cannot_grant_credits() {
    for corruption in [
        "command",
        "async-id",
        "async-flag",
        "direction",
        "async-context",
    ] {
        let Fixture {
            handler,
            worker,
            sender,
            ..
        } = fixture().await;
        let sent = handler
            .sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into()))
            .await
            .unwrap();
        let mut response = async_echo(smb_msg::Status::Pending, 1);
        match corruption {
            "command" => {
                response[12..14].copy_from_slice(&(smb_msg::Command::Read as u16).to_le_bytes())
            }
            "async-id" => response[32..40].fill(0),
            "async-flag" => response[16..20].copy_from_slice(&1_u32.to_le_bytes()),
            "direction" => response[16..20].copy_from_slice(&2_u32.to_le_bytes()),
            "async-context" => (), // Valid packet, but asynchronous replies are disallowed.
            _ => unreachable!(),
        }
        sender.send(response).await.unwrap();
        let result = handler
            .recvo(
                ReceiveOptions::new()
                    .with_cmd(Some(smb_msg::Command::Echo))
                    .with_msg_id_filter(sent.msg_id)
                    .with_allow_async(corruption != "async-context"),
            )
            .await;
        worker.stop().await.unwrap();
        assert!(result.is_err(), "{corruption} was accepted");
        if corruption == "async-context" {
            assert!(matches!(&result, Err(Error::InvalidArgument(reason))
                if reason.contains("not allowed in this context")));
        }
        assert_eq!(
            handler.curr_credits.available_permits(),
            0,
            "{corruption} granted an invalid credit"
        );
        assert_eq!(
            handler.credit_pool.load(Ordering::SeqCst),
            1,
            "{corruption} altered the credit pool"
        );
    }
}

#[tokio::test]
async fn ordinary_collision_error_returns_credits_before_reporting_failure() {
    let Fixture {
        handler,
        worker,
        sender,
        ..
    } = fixture().await;
    let sent = handler
        .sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into()))
        .await
        .unwrap();
    let mut response = async_echo(smb_msg::Status::ObjectNameCollision, 1);
    response[16..20].copy_from_slice(&1_u32.to_le_bytes()); // synchronous response
    response[32..40].fill(0);
    sender.send(response).await.unwrap();
    let result = handler
        .recvo(
            ReceiveOptions::new()
                .with_cmd(Some(smb_msg::Command::Echo))
                .with_msg_id_filter(sent.msg_id),
        )
        .await;
    assert!(matches!(result, Err(Error::ReceivedErrorMessage(status, _))
        if status == smb_msg::Status::ObjectNameCollision as u32));
    assert_eq!(handler.curr_credits.available_permits(), 1);
    let next = tokio::time::timeout(
        Duration::from_millis(50),
        handler.sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into())),
    )
    .await;
    worker.stop().await.unwrap();
    assert!(
        matches!(next, Ok(Ok(_))),
        "an ordinary collision must not block the next request"
    );
}

#[tokio::test]
async fn burst_pending_and_final_preserve_credit_grant_before_receive_registration() {
    let Fixture {
        handler,
        worker,
        sender,
        reads,
        events,
    } = fixture().await;
    let sent = handler
        .sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into()))
        .await
        .unwrap();
    sender
        .send(async_echo(smb_msg::Status::Pending, 1))
        .await
        .unwrap();
    sender
        .send(async_echo(smb_msg::Status::Success, 0))
        .await
        .unwrap();
    // Let both real incoming-data callbacks complete before registering the
    // response waiter. A map holding only one packet per ID loses Pending here.
    tokio::time::timeout(Duration::from_secs(1), async {
        while reads.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let response = handler
        .recvo(
            ReceiveOptions::new()
                .with_msg_id_filter(sent.msg_id)
                .with_allow_async(true),
        )
        .await
        .unwrap();
    assert_eq!(response.message.header.credit_request, 0);
    let credits = handler.curr_credits.available_permits();
    let next = tokio::time::timeout(
        Duration::from_millis(50),
        handler.sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into())),
    )
    .await;
    worker.stop().await.unwrap();
    eprintln!(
        "burst peer events: {:?}; available credits={credits}; next={next:?}",
        events.lock().unwrap()
    );
    assert_eq!(
        credits, 1,
        "queued Pending must not be overwritten by its same-ID final response"
    );
    assert_eq!(
        handler.credit_pool.load(Ordering::SeqCst),
        1,
        "queued replies must charge one request once"
    );
    assert!(
        matches!(next, Ok(Ok(_))),
        "the next request lost the queued interim credit"
    );
}

#[tokio::test]
async fn pending_then_collision_error_keeps_credit_for_winner_reconciliation() {
    let Fixture {
        handler,
        worker,
        sender,
        reads,
        ..
    } = fixture().await;
    let sent = handler
        .sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into()))
        .await
        .unwrap();
    sender
        .send(async_echo(smb_msg::Status::Pending, 1))
        .await
        .unwrap();
    sender
        .send(async_echo(smb_msg::Status::ObjectNameCollision, 0))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while reads.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let result = handler
        .recvo(
            ReceiveOptions::new()
                .with_msg_id_filter(sent.msg_id)
                .with_allow_async(true),
        )
        .await;
    assert!(matches!(result, Err(Error::ReceivedErrorMessage(status, _))
        if status == smb_msg::Status::ObjectNameCollision as u32));
    assert_eq!(handler.curr_credits.available_permits(), 1);
    assert_eq!(handler.credit_pool.load(Ordering::SeqCst), 1);
    let next = tokio::time::timeout(
        Duration::from_millis(50),
        handler.sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into())),
    )
    .await;
    worker.stop().await.unwrap();
    assert!(
        matches!(next, Ok(Ok(_))),
        "a deferred collision must leave credit for the next stat/read"
    );
}

#[tokio::test]
async fn legacy_credit_window_releases_once_for_interim_and_final() {
    let Fixture {
        handler,
        worker,
        sender,
        reads,
        ..
    } = fixture_with_large_mtu(false).await;
    let sent = handler
        .sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into()))
        .await
        .unwrap();
    for (status, credits) in [(smb_msg::Status::Pending, 1), (smb_msg::Status::Success, 0)] {
        let mut response = async_echo(status, credits);
        response[6..8].fill(0);
        sender.send(response).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while reads.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    handler
        .recvo(
            ReceiveOptions::new()
                .with_msg_id_filter(sent.msg_id)
                .with_allow_async(true),
        )
        .await
        .unwrap();
    assert_eq!(
        handler.curr_credits.available_permits(),
        1,
        "one request must release exactly one legacy permit"
    );
    let next = tokio::time::timeout(
        Duration::from_millis(50),
        handler.sendo(OutgoingMessage::new(smb_msg::EchoRequest::default().into())),
    )
    .await;
    worker.stop().await.unwrap();
    assert!(matches!(next, Ok(Ok(_))));
}
