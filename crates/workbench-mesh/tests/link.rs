//! The handshake and the encrypted link, end to end over loopback TCP.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use workbench_core::mesh::StepJob;
use workbench_core::schemas::{FileKind, InputFile, TaskStep, ToolResult};
use workbench_core::TurnMode;
use workbench_mesh::code::{PairingCode, PairingWindow, DEFAULT_TTL};
use workbench_mesh::frame::{FileBlob, Frame, Hello};
use workbench_mesh::identity::{DeviceIdentity, TrustStore};
use workbench_mesh::link::{accept, pair, reconnect, Accepted};
use workbench_mesh::{MeshError, PROTOCOL_VERSION};

struct Host {
    me: DeviceIdentity,
    window: Arc<Mutex<Option<PairingWindow>>>,
    trust: Arc<Mutex<TrustStore>>,
}

impl Host {
    fn new(window: Option<PairingWindow>) -> Self {
        Self {
            me: DeviceIdentity::generate("host-laptop").unwrap(),
            window: Arc::new(Mutex::new(window)),
            trust: Arc::new(Mutex::new(TrustStore::in_memory())),
        }
    }

    fn code(&self) -> PairingCode {
        self.window.lock().unwrap().as_ref().unwrap().code().clone()
    }

    /// Listen on loopback and run the Host handshake for one connection.
    async fn accept_one(
        &self,
    ) -> (SocketAddr, tokio::task::JoinHandle<workbench_mesh::Result<Accepted>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (me, window, trust) = (self.me.clone(), self.window.clone(), self.trust.clone());
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let hello = Hello { name: me.name.clone(), capabilities: serde_json::json!({}) };
            accept(tcp, &me, &hello, &window, &trust).await
        });
        (addr, task)
    }
}

fn window(max_failures: u8) -> Option<PairingWindow> {
    Some(PairingWindow::open(PairingCode::generate(), DEFAULT_TTL, max_failures))
}

fn worker() -> (DeviceIdentity, Hello) {
    let me = DeviceIdentity::generate("gpu-workstation").unwrap();
    let hello = Hello { name: me.name.clone(), capabilities: serde_json::json!({ "gpu": "RTX" }) };
    (me, hello)
}

/// Same public tag, different secret: reaches the Host's key exchange and fails there.
fn wrong_secret(code: &PairingCode) -> PairingCode {
    // "K7Q-4M9": the tag is "K7", so changing the last three keeps the tag.
    let shown = code.to_string();
    let guess = if shown.ends_with("000") { "111" } else { "000" };
    PairingCode::parse(&format!("{}{}", &shown[..4], guess)).unwrap()
}

fn rejected(r: workbench_mesh::Result<impl Sized>) -> String {
    match r {
        Err(MeshError::Rejected(reason)) => reason,
        Err(e) => panic!("expected a refusal, got {e}"),
        Ok(_) => panic!("expected a refusal, got a link"),
    }
}

#[tokio::test]
async fn the_right_code_pairs_both_sides_and_large_frames_flow_both_ways() {
    let host = Host::new(window(5));
    let (addr, task) = host.accept_one().await;
    let (me, hello) = worker();

    let mut w = pair(addr, &host.code(), &me, &hello).await.unwrap();
    let Accepted { link: mut h, newly_paired } = task.await.unwrap().unwrap();

    assert!(newly_paired, "the Host must show consent before trusting it");
    assert_eq!(w.peer().public_key, host.me.public_key, "worker pinned the Host's real key");
    assert_eq!(h.peer().public_key, me.public_key);
    assert_eq!(h.peer().name, "gpu-workstation");
    assert_eq!(h.peer().capabilities["gpu"], "RTX");

    // 3 MB of attachment: many Noise chunks.
    let bytes: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let job = StepJob {
        step: TaskStep {
            id: "p1".into(),
            task: "parse_pdf".into(),
            args: serde_json::Value::Null,
            depends_on: vec![],
        },
        dep_outputs: Default::default(),
        files: vec![InputFile { path: "a.pdf".into(), kind: FileKind::Pdf, original_name: "a.pdf".into() }],
        prompt: "what is wrong with the pump?".into(),
        session_blob: String::new(),
        mode: TurnMode::Fast,
        persona: String::new(),
    };
    let blob = FileBlob { original_name: "a.pdf".into(), kind: FileKind::Pdf, bytes: bytes.clone() };
    // Send and receive at once, as the hub's writer and the worker's reader do:
    // ~4 MB on the wire is more than a socket buffers, so sending first and
    // reading after would deadlock.
    let job_frame = Frame::Job { job_id: "j1".into(), job, files: vec![blob] };
    let (sent, received) = tokio::join!(h.send(&job_frame), w.recv());
    sent.unwrap();

    match received.unwrap() {
        Frame::Job { job_id, job, files } => {
            assert_eq!(job_id, "j1");
            assert_eq!(job.prompt, "what is wrong with the pump?");
            assert_eq!(files[0].bytes, bytes);
        }
        other => panic!("expected a job, got {other:?}"),
    }

    let result = ToolResult {
        step_id: "p1".into(),
        task: "parse_pdf".into(),
        ok: true,
        data: serde_json::json!({ "text": "corrosion" }),
        error: None,
        warning: None,
        elapsed_ms: 5,
    };
    w.send(&Frame::Done { job_id: "j1".into(), result, timings: vec![] }).await.unwrap();
    assert!(matches!(h.recv().await.unwrap(), Frame::Done { result, .. } if result.data["text"] == "corrosion"));
}

#[tokio::test]
async fn wrong_codes_are_refused_and_use_up_the_window() {
    let host = Host::new(window(2));
    let code = host.code();
    let (me, hello) = worker();

    for _ in 0..2 {
        let (addr, task) = host.accept_one().await;
        assert_eq!(rejected(pair(addr, &wrong_secret(&code), &me, &hello).await), "wrong code");
        assert!(task.await.unwrap().is_err());
    }

    // Even the right code is refused now: five guesses, not five million.
    let (addr, _task) = host.accept_one().await;
    assert!(rejected(pair(addr, &code, &me, &hello).await).contains("too many wrong attempts"));
}

#[tokio::test]
async fn a_wrong_tag_is_refused_without_spending_an_attempt() {
    let host = Host::new(window(1));
    let (me, hello) = worker();
    // Another Host's code: same secret, first tag character changed. Derived,
    // not random: two random 2-character tags collide about 1 run in 1,024.
    let shown = host.code().to_string();
    let first = if shown.starts_with('0') { '1' } else { '0' };
    let other_host = PairingCode::parse(&format!("{first}{}", &shown[1..])).unwrap();
    assert_ne!(other_host.tag(), host.code().tag());

    let (addr, _task) = host.accept_one().await;
    assert_eq!(rejected(pair(addr, &other_host, &me, &hello).await), "wrong code");

    let (addr, task) = host.accept_one().await;
    pair(addr, &host.code(), &me, &hello).await.expect("the one attempt is still there");
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test]
async fn an_expired_code_or_no_code_is_refused_with_the_reason() {
    let (me, hello) = worker();

    let expired = Host::new(Some(PairingWindow::open(PairingCode::generate(), Duration::ZERO, 5)));
    let (addr, _t) = expired.accept_one().await;
    assert!(rejected(pair(addr, &expired.code(), &me, &hello).await).contains("expired"));

    let closed = Host::new(None);
    let (addr, _t) = closed.accept_one().await;
    let reason = rejected(pair(addr, &PairingCode::generate(), &me, &hello).await);
    assert!(reason.contains("isn't accepting new devices"), "{reason}");
}

#[tokio::test]
async fn a_paired_device_reconnects_without_a_code_and_strangers_cannot() {
    let host = Host::new(None);
    let (me, hello) = worker();
    host.trust.lock().unwrap().add(&me.name, &me.public_key).unwrap();

    let (addr, task) = host.accept_one().await;
    let w = reconnect(addr, &me, &host.me.public_key, &hello).await.unwrap();
    let accepted = task.await.unwrap().unwrap();
    assert!(!accepted.newly_paired);
    assert_eq!(w.peer().public_key, host.me.public_key);

    // A device the Host never paired with (or removed).
    let (stranger, stranger_hello) = worker();
    let (addr, _t) = host.accept_one().await;
    let reason = rejected(reconnect(addr, &stranger, &host.me.public_key, &stranger_hello).await);
    assert!(reason.contains("isn't paired"), "{reason}");

    // Something else answering at the Host's old address.
    let (addr, _t) = host.accept_one().await;
    let imposter_key = DeviceIdentity::generate("x").unwrap().public_key;
    let reason = rejected(reconnect(addr, &me, &imposter_key, &hello).await);
    assert!(reason.contains("isn't the Host"), "{reason}");
}

#[tokio::test]
async fn a_protocol_mismatch_is_named_on_both_sides() {
    let host = Host::new(window(5));
    let (addr, task) = host.accept_one().await;

    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(format!("ws://{addr}/mesh"), tcp).await.unwrap();
    ws.send(Message::text(r#"{"proto":99,"mode":"pair","tag":"ABC"}"#)).await.unwrap();

    match task.await.unwrap() {
        Err(MeshError::ProtocolMismatch { ours, theirs }) => assert_eq!((ours, theirs), (PROTOCOL_VERSION, 99)),
        Err(e) => panic!("expected a mismatch, got {e}"),
        Ok(_) => panic!("expected a mismatch, got a link"),
    }
    let Some(Ok(Message::Text(text))) = ws.next().await else { panic!("expected a refusal") };
    assert!(text.as_str().contains(&format!("\"proto\":{PROTOCOL_VERSION}")));
}

/// Security review fix A: the failure budget held per attempt, so a burst of
/// simultaneous guesses gets no more tries than one guess after another.
#[tokio::test]
async fn a_burst_of_simultaneous_guesses_gets_no_more_than_the_budget() {
    let host = Host::new(window(3));
    let code = host.code();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (me_h, window_h, trust_h) = (host.me.clone(), host.window.clone(), host.trust.clone());
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let (me, window, trust) = (me_h.clone(), window_h.clone(), trust_h.clone());
            tokio::spawn(async move {
                let hello = Hello { name: me.name.clone(), capabilities: serde_json::json!({}) };
                let _ = accept(tcp, &me, &hello, &window, &trust).await;
            });
        }
    });

    let guesses = (0..12).map(|_| {
        let wrong = wrong_secret(&code);
        async move {
            let (me, hello) = worker();
            match pair(addr, &wrong, &me, &hello).await {
                Err(MeshError::Rejected(reason)) => reason,
                other => panic!("a wrong code can't pair: {:?}", other.err()),
            }
        }
    });
    let reasons = futures_util::future::join_all(guesses).await;

    let reached_the_key_exchange = reasons.iter().filter(|r| *r == "wrong code").count();
    assert!(reached_the_key_exchange <= 3, "{reached_the_key_exchange} guesses got through: {reasons:?}");
    let (me, hello) = worker();
    assert!(rejected(pair(addr, &code, &me, &hello).await).contains("too many wrong attempts"));
}

/// Fix B: before the peer has proved anything, a message is at most ~64 KiB,
/// not tungstenite's default 64 MB.
#[tokio::test]
async fn an_unauthenticated_device_cannot_send_a_huge_message() {
    let host = Host::new(window(5));
    let (addr, task) = host.accept_one().await;
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(format!("ws://{addr}/mesh"), tcp).await.unwrap();
    let _ = ws.send(Message::text("x".repeat(2 * 1024 * 1024))).await;

    let outcome = tokio::time::timeout(Duration::from_secs(5), task).await.expect("refused promptly");
    assert!(outcome.unwrap().is_err(), "a 2 MB opening message must be refused");
}

/// Fix F: the other device names itself; that name is bounded and printable.
#[tokio::test]
async fn a_hostile_device_name_is_trimmed() {
    let host = Host::new(window(5));
    let (addr, task) = host.accept_one().await;
    let me = DeviceIdentity::generate("x").unwrap();
    let hello = Hello { name: format!("  evil\u{7}{}", "A".repeat(500)), capabilities: serde_json::json!({}) };
    let _w = pair(addr, &host.code(), &me, &hello).await.unwrap();
    let accepted = task.await.unwrap().unwrap();
    let name = &accepted.link.peer().name;
    assert_eq!(name.chars().count(), 64);
    assert!(name.starts_with("evilAAA"), "{name}");
}

/// Fix G: with several candidates (a tag collision, an impostor, a dead
/// address), the Worker tries each until the real Host pairs.
#[tokio::test]
async fn the_worker_tries_every_candidate_until_one_pairs() {
    let host = Host::new(window(5));
    // Nothing listens here once the listener is dropped: connection refused.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let (real, task) = host.accept_one().await;
    let (me, hello) = worker();

    let link = workbench_mesh::link::pair_any(&[dead, real], &host.code(), &me, &hello).await.unwrap();
    assert_eq!(link.peer().public_key, host.me.public_key);
    assert!(task.await.unwrap().is_ok());
}

/// Needs multicast on a real interface, which CI sandboxes often block.
/// Run by hand: `cargo test -p workbench-mesh --test link -- --ignored`.
#[tokio::test]
#[ignore = "needs LAN multicast"]
async fn a_worker_finds_the_host_by_its_tag() {
    let ad = workbench_mesh::discovery::Advertisement::start("trustcore-test-host", 48123, "K7").unwrap();
    let found = workbench_mesh::discovery::find_hosts("K7", Duration::from_secs(5)).await.unwrap();
    assert!(found.iter().any(|a| a.port() == 48123), "{found:?}");
    let none = workbench_mesh::discovery::find_hosts("ZZ", Duration::from_secs(2)).await.unwrap();
    assert!(none.is_empty());
    let by_id = workbench_mesh::discovery::find_host_by_id("trustcore-test-host", Duration::from_secs(5)).await.unwrap();
    assert!(by_id.iter().any(|a| a.port() == 48123), "a paired Worker finds its Host without a code");
    drop(ad);
}
