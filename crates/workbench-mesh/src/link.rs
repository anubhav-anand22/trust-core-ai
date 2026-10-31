//! The handshake, and then encrypted frames over a WebSocket.
//!
//! A Worker always dials the Host (MESH-1). The first WebSocket message is a
//! plaintext `Opening` naming the protocol version and the mode:
//!
//! ```text
//! pair:       Opening{pair, tag} → SPAKE2(code secret) → Noise XXpsk3 (psk = SPAKE2 key)
//! reconnect:  Opening{reconnect} → Noise XX → each side checks the other's key is one it stored
//! then:       Hello ⇄ Hello → frames
//! ```
//!
//! **Why SPAKE2 before Noise.** A 6-character secret used directly as a Noise
//! PSK could be brute-forced offline by anyone who sat in the middle of one
//! handshake. SPAKE2 turns it into a 32-byte key so each guess costs a live
//! handshake, and the Host's [`PairingWindow`] allows five failures. A wrong code
//! makes the two SPAKE2 keys differ, so the Host's read of the last Noise message
//! fails: that is the one place a wrong code is detected.
//!
//! **Why Noise and not TLS.** Both sides authenticate with keys they pinned at
//! pairing; there is no certificate authority on an air-gapped LAN to ask.
//!
//! After the handshake, every frame is JSON, encrypted as one Noise message for
//! an 8-byte length, then as many ≤64 KiB Noise messages as the body needs.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::code::{PairingCode, PairingWindow};
use crate::frame::{Frame, Hello};
use crate::identity::{device_id, DeviceIdentity, TrustStore, KEY_PARAMS};
use crate::{MeshError, Result, PROTOCOL_VERSION};

const PAIR_PARAMS: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";
/// Binds the SPAKE2 exchange to this protocol, so a key from another app's
/// SPAKE2 can never be replayed here.
const SPAKE_ID: &[u8] = b"trustcore-mesh-v1";

/// A whole handshake must finish within this, or a silent client could hold a
/// pairing attempt open forever.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// The Noise spec caps one message at 65535 bytes, 16 of them the auth tag
/// (snow keeps its own constants private).
const NOISE_MAX: usize = 65535;
const CHUNK: usize = NOISE_MAX - 16;

/// The largest frame either side accepts: big attachments, not unbounded memory.
pub const MAX_FRAME_BYTES: u64 = 256 * 1024 * 1024;

type Ws = WebSocketStream<TcpStream>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Pair,
    Reconnect,
}

/// The one plaintext message a Worker sends.
#[derive(Serialize, Deserialize)]
struct Opening {
    proto: u32,
    mode: Mode,
    /// The code's public tag, so the Host can say "wrong code" before any crypto.
    #[serde(default)]
    tag: Option<String>,
}

/// Sent in plaintext instead of the next handshake message, then the socket closes.
#[derive(Serialize, Deserialize)]
struct Refusal {
    error: String,
    #[serde(default)]
    proto: Option<u32>,
}

/// The device at the other end, as proven by the handshake.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub name: String,
    pub device_id: String,
    pub public_key: Vec<u8>,
    pub capabilities: serde_json::Value,
}

/// An authenticated, encrypted connection to one device.
pub struct Link {
    writer: LinkWriter,
    reader: LinkReader,
    peer: Peer,
}

impl Link {
    pub fn peer(&self) -> &Peer {
        &self.peer
    }

    pub async fn send(&mut self, frame: &Frame) -> Result<()> {
        self.writer.send(frame).await
    }

    pub async fn recv(&mut self) -> Result<Frame> {
        self.reader.recv().await
    }

    /// Separate halves, so one task can wait for frames while another sends.
    pub fn split(self) -> (LinkWriter, LinkReader, Peer) {
        (self.writer, self.reader, self.peer)
    }
}

pub struct LinkWriter {
    sink: SplitSink<Ws, Message>,
    noise: Arc<Mutex<snow::TransportState>>,
}

pub struct LinkReader {
    stream: SplitStream<Ws>,
    noise: Arc<Mutex<snow::TransportState>>,
}

impl LinkWriter {
    pub async fn send(&mut self, frame: &Frame) -> Result<()> {
        let body = serde_json::to_vec(frame)?;
        if body.len() as u64 > MAX_FRAME_BYTES {
            return Err(MeshError::Protocol(format!(
                "frame of {} bytes is over the {MAX_FRAME_BYTES}-byte limit",
                body.len()
            )));
        }
        let header = (body.len() as u64).to_be_bytes();
        let sealed = std::iter::once(header.as_slice())
            .chain(body.chunks(CHUNK))
            .map(|part| self.seal(part))
            .collect::<Result<Vec<_>>>()?;
        for msg in sealed {
            self.sink.send(Message::binary(msg)).await?;
        }
        Ok(())
    }

    pub async fn close(mut self, reason: &str) {
        let _ = self.send(&Frame::Bye { reason: reason.to_string() }).await;
        let _ = self.sink.close().await;
    }

    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>> {
        let mut out = vec![0u8; NOISE_MAX];
        // Never held across an await: encryption is synchronous and fast.
        let n = self.noise.lock().expect("noise lock").write_message(plain, &mut out)?;
        out.truncate(n);
        Ok(out)
    }
}

impl LinkReader {
    pub async fn recv(&mut self) -> Result<Frame> {
        let header = self.open_next().await?;
        let len = u64::from_be_bytes(
            header
                .as_slice()
                .try_into()
                .map_err(|_| MeshError::Protocol("bad frame header".into()))?,
        );
        if len > MAX_FRAME_BYTES {
            return Err(MeshError::Protocol(format!("peer announced a {len}-byte frame")));
        }
        // Grow as chunks arrive: a header alone must not reserve 256 MB.
        let mut body = Vec::with_capacity((len as usize).min(1024 * 1024));
        while (body.len() as u64) < len {
            let part = self.open_next().await?;
            if part.is_empty() || body.len() as u64 + part.len() as u64 > len {
                return Err(MeshError::Protocol("frame body does not match its header".into()));
            }
            body.extend_from_slice(&part);
        }
        Ok(serde_json::from_slice(&body)?)
    }

    async fn open_next(&mut self) -> Result<Vec<u8>> {
        let sealed = next_binary(&mut self.stream).await?;
        let mut out = vec![0u8; NOISE_MAX];
        let n = self.noise.lock().expect("noise lock").read_message(&sealed, &mut out)?;
        out.truncate(n);
        Ok(out)
    }
}

// --- Host side -----------------------------------------------------------------

/// A Worker that completed the handshake.
pub struct Accepted {
    pub link: Link,
    /// `true` when it paired with the code just now. The Host must show the
    /// consent screen and only then [`TrustStore::add`] it (MESH-4); until then
    /// it gets no work.
    pub newly_paired: bool,
}

/// Run the Host side of the handshake on one incoming TCP connection.
///
/// `window` is the open pairing code, if any. `trust` is the paired devices. A
/// failed pairing attempt is recorded on `window` here.
pub async fn accept(
    tcp: TcpStream,
    me: &DeviceIdentity,
    hello: &Hello,
    window: &Mutex<Option<PairingWindow>>,
    trust: &Mutex<TrustStore>,
) -> Result<Accepted> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, accept_inner(tcp, me, hello, window, trust))
        .await
        .map_err(|_| MeshError::Timeout("handshake"))?
}

async fn accept_inner(
    tcp: TcpStream,
    me: &DeviceIdentity,
    hello: &Hello,
    window: &Mutex<Option<PairingWindow>>,
    trust: &Mutex<TrustStore>,
) -> Result<Accepted> {
    let mut ws = tokio_tungstenite::accept_async_with_config(tcp, Some(ws_config())).await?;

    let opening: Opening = serde_json::from_str(&next_text(&mut ws).await?)?;
    if opening.proto != PROTOCOL_VERSION {
        refuse(&mut ws, "protocol version mismatch").await;
        return Err(MeshError::ProtocolMismatch { ours: PROTOCOL_VERSION, theirs: opening.proto });
    }

    match opening.mode {
        Mode::Pair => {
            // A wrong tag is refused for free (the tag is public). Past this
            // point the attempt holds a unit of the failure budget until it ends.
            let attempt = {
                let mut guard = window.lock().expect("pairing lock");
                match guard.as_mut() {
                    None => Err("this Host isn't accepting new devices right now"),
                    Some(w) if Some(w.code().tag()) != opening.tag.as_deref() => Err("wrong code"),
                    Some(w) => w.begin().map(|()| w.code().clone()).map_err(|c| c.reason()),
                }
            };
            let code = match attempt {
                Ok(code) => code,
                Err(reason) => {
                    refuse(&mut ws, reason).await;
                    return Err(MeshError::Rejected(reason.into()));
                }
            };
            // Spends the unit on every way out except a completed pairing,
            // including a timeout or a dropped connection.
            let mut attempt = Attempt { window, code: code.clone(), paired: false };

            let psk = spake(&mut ws, &code).await?;
            let hs = snow::Builder::new(PAIR_PARAMS.parse()?)
                .local_private_key(&me.private_key)?
                .psk(3, &psk)?
                .build_responder()?;
            let hs = match noise(&mut ws, hs).await {
                Ok(hs) => hs,
                Err(MeshError::Noise(_)) => {
                    refuse(&mut ws, "wrong code").await;
                    return Err(MeshError::Rejected("wrong code".into()));
                }
                Err(e) => return Err(e),
            };
            let link = finish(ws, hs, hello).await?;
            attempt.paired = true;
            tracing::info!(device = %link.peer.device_id, name = %link.peer.name, "mesh: device paired");
            Ok(Accepted { link, newly_paired: true })
        }
        Mode::Reconnect => {
            let hs = snow::Builder::new(KEY_PARAMS.parse()?)
                .local_private_key(&me.private_key)?
                .build_responder()?;
            let hs = noise(&mut ws, hs).await?;
            let known = hs
                .get_remote_static()
                .is_some_and(|key| trust.lock().expect("trust lock").trusts(key).is_some());
            if !known {
                let reason = "this device isn't paired with this Host; pair again with a new code";
                refuse(&mut ws, reason).await;
                return Err(MeshError::Rejected(reason.into()));
            }
            let link = finish(ws, hs, hello).await?;
            tracing::info!(device = %link.peer.device_id, "mesh: paired device reconnected");
            Ok(Accepted { link, newly_paired: false })
        }
    }
}

/// One pairing attempt's hold on the window's failure budget.
struct Attempt<'a> {
    window: &'a Mutex<Option<PairingWindow>>,
    code: PairingCode,
    paired: bool,
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if let Some(w) = self.window.lock().expect("pairing lock").as_mut() {
            // A newer code has its own budget: don't touch it.
            if *w.code() == self.code {
                w.finish(self.paired);
            }
        }
    }
}

/// Every message on a link is small: the handshake's few hundred bytes, then
/// Noise chunks of at most 64 KiB. tungstenite would otherwise accept 64 MB
/// messages, and the first one arrives before the peer has proved anything.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_MESSAGE))
        .max_frame_size(Some(MAX_WS_MESSAGE))
}

const MAX_WS_MESSAGE: usize = NOISE_MAX + 1024;

/// A display name from the other device, trimmed and bounded: it goes into
/// the UI and the logs.
const MAX_NAME_CHARS: usize = 64;

// --- Worker side ---------------------------------------------------------------

/// Pair with a Host using its code. Store `link.peer().public_key` afterwards:
/// it is what [`reconnect`] checks next time.
pub async fn pair(addr: SocketAddr, code: &PairingCode, me: &DeviceIdentity, hello: &Hello) -> Result<Link> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut ws = dial(addr, Mode::Pair, Some(code.tag())).await?;
        let psk = spake(&mut ws, code).await?;
        let hs = snow::Builder::new(PAIR_PARAMS.parse()?)
            .local_private_key(&me.private_key)?
            .psk(3, &psk)?
            .build_initiator()?;
        let hs = noise(&mut ws, hs).await?;
        // A wrong code only shows here: the Host refuses instead of sending Hello.
        finish(ws, hs, hello).await
    })
    .await
    .map_err(|_| MeshError::Timeout("pairing"))?
}

/// Reconnect to a Host this device paired with before.
pub async fn reconnect(
    addr: SocketAddr,
    me: &DeviceIdentity,
    host_public_key: &[u8],
    hello: &Hello,
) -> Result<Link> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut ws = dial(addr, Mode::Reconnect, None).await?;
        let hs = snow::Builder::new(KEY_PARAMS.parse()?)
            .local_private_key(&me.private_key)?
            .build_initiator()?;
        let hs = noise(&mut ws, hs).await?;
        if hs.get_remote_static() != Some(host_public_key) {
            refuse(&mut ws, "this isn't the Host this device paired with").await;
            return Err(MeshError::Rejected(
                "the device at that address isn't the Host this device paired with".into(),
            ));
        }
        finish(ws, hs, hello).await
    })
    .await
    .map_err(|_| MeshError::Timeout("reconnect"))?
}

/// [`pair`] with each of `addrs` in turn until one accepts the code.
///
/// Discovery can return several: two Hosts sharing a 2-character tag (about 1
/// in 1,024), or a device advertising a tag it doesn't own. A wrong one just
/// fails its handshake, so trying the next is safe. Returns the last error.
pub async fn pair_any(addrs: &[SocketAddr], code: &PairingCode, me: &DeviceIdentity, hello: &Hello) -> Result<Link> {
    let mut last = MeshError::Rejected("no Host to try".into());
    for &addr in addrs {
        match pair(addr, code, me, hello).await {
            Ok(link) => return Ok(link),
            Err(e) => {
                tracing::warn!(%addr, error = %e, "mesh: that device didn't pair; trying the next");
                last = e;
            }
        }
    }
    Err(last)
}

/// [`reconnect`] with each of `addrs` in turn. Anyone can advertise a Host's
/// public device id; only the real Host can prove its key.
pub async fn reconnect_any(
    addrs: &[SocketAddr],
    me: &DeviceIdentity,
    host_public_key: &[u8],
    hello: &Hello,
) -> Result<Link> {
    let mut last = MeshError::Rejected("no Host to try".into());
    for &addr in addrs {
        match reconnect(addr, me, host_public_key, hello).await {
            Ok(link) => return Ok(link),
            Err(e) => {
                tracing::warn!(%addr, error = %e, "mesh: that device wasn't our Host; trying the next");
                last = e;
            }
        }
    }
    Err(last)
}

async fn dial(addr: SocketAddr, mode: Mode, tag: Option<&str>) -> Result<Ws> {
    let tcp = TcpStream::connect(addr).await?;
    let (mut ws, _) =
        tokio_tungstenite::client_async_with_config(format!("ws://{addr}/mesh"), tcp, Some(ws_config())).await?;
    let opening = Opening { proto: PROTOCOL_VERSION, mode, tag: tag.map(str::to_string) };
    ws.send(Message::text(serde_json::to_string(&opening)?)).await?;
    Ok(ws)
}

// --- shared steps ----------------------------------------------------------------

/// Symmetric SPAKE2: both sides send, then read. Same code → same 32-byte key.
async fn spake(ws: &mut Ws, code: &PairingCode) -> Result<[u8; 32]> {
    let (state, outbound) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.secret().as_bytes()),
        &Identity::new(SPAKE_ID),
    );
    ws.send(Message::binary(outbound)).await?;
    let inbound = next_binary(ws).await?;
    let key = state
        .finish(&inbound)
        .map_err(|e| MeshError::Protocol(format!("pairing key exchange: {e:?}")))?;
    key.try_into().map_err(|_| MeshError::Protocol("pairing key has the wrong length".into()))
}

/// Drive a Noise handshake to completion, whichever side we are.
async fn noise(ws: &mut Ws, mut hs: snow::HandshakeState) -> Result<snow::HandshakeState> {
    let mut buf = vec![0u8; NOISE_MAX];
    while !hs.is_handshake_finished() {
        if hs.is_my_turn() {
            let n = hs.write_message(&[], &mut buf)?;
            ws.send(Message::binary(buf[..n].to_vec())).await?;
        } else {
            let msg = next_binary(ws).await?;
            hs.read_message(&msg, &mut buf)?;
        }
    }
    Ok(hs)
}

/// Switch to transport mode and swap `Hello`s.
async fn finish(ws: Ws, hs: snow::HandshakeState, hello: &Hello) -> Result<Link> {
    let remote_key = hs
        .get_remote_static()
        .ok_or_else(|| MeshError::Protocol("peer sent no static key".into()))?
        .to_vec();
    let noise = Arc::new(Mutex::new(hs.into_transport_mode()?));
    let (sink, stream) = ws.split();
    let mut writer = LinkWriter { sink, noise: noise.clone() };
    let mut reader = LinkReader { stream, noise };

    // On a wrong code the Host refuses and closes while our Hello is in flight,
    // so the send can fail with a reset. The refusal is the useful error: read
    // for it before reporting the send failure.
    let sent = writer.send(&Frame::Hello(hello.clone())).await;
    let theirs = match reader.recv().await {
        Ok(Frame::Hello(h)) => h,
        Ok(other) => return Err(MeshError::Protocol(format!("expected Hello, got {other:?}"))),
        Err(e @ (MeshError::Rejected(_) | MeshError::ProtocolMismatch { .. })) => return Err(e),
        Err(e) => return Err(sent.err().unwrap_or(e)),
    };
    sent?;
    let name: String = theirs.name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    let peer = Peer {
        name: if name.is_empty() { "unnamed device".into() } else { name },
        device_id: device_id(&remote_key),
        public_key: remote_key,
        capabilities: theirs.capabilities,
    };
    Ok(Link { writer, reader, peer })
}

async fn refuse(ws: &mut Ws, reason: &str) {
    let msg = Refusal { error: reason.to_string(), proto: Some(PROTOCOL_VERSION) };
    if let Ok(text) = serde_json::to_string(&msg) {
        let _ = ws.send(Message::text(text)).await;
    }
    let _ = ws.close(None).await;
}

/// The next binary message. A plaintext refusal becomes [`MeshError::Rejected`].
async fn next_binary<S>(stream: &mut S) -> Result<Vec<u8>>
where
    S: futures_util::Stream<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            None | Some(Ok(Message::Close(_))) => return Err(MeshError::Closed),
            Some(Ok(Message::Binary(bytes))) => return Ok(bytes.to_vec()),
            Some(Ok(Message::Text(text))) => return Err(refusal(text.as_str())),
            Some(Ok(_)) => continue, // ping/pong: tungstenite answers pings itself
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

async fn next_text(ws: &mut Ws) -> Result<String> {
    loop {
        match ws.next().await {
            None | Some(Ok(Message::Close(_))) => return Err(MeshError::Closed),
            Some(Ok(Message::Text(text))) => return Ok(text.as_str().to_string()),
            Some(Ok(Message::Binary(_))) => {
                return Err(MeshError::Protocol("expected the opening message".into()))
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

fn refusal(text: &str) -> MeshError {
    match serde_json::from_str::<Refusal>(text) {
        Ok(r) => match r.proto {
            Some(theirs) if theirs != PROTOCOL_VERSION => {
                MeshError::ProtocolMismatch { ours: PROTOCOL_VERSION, theirs }
            }
            _ => MeshError::Rejected(r.error),
        },
        Err(_) => MeshError::Protocol("unexpected plaintext message".into()),
    }
}
