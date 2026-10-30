//! Direct transfer of a finished snapshot to another computer on the local network.
//!
//! The receiving computer listens only while its Receive screen is open, announces itself over
//! mDNS, and shows an 8-digit pairing code. The sender types that code and both sides run SPAKE2
//! with it: without the code nobody can complete the exchange, and each connection allows one
//! guess (the receiver stops after a few wrong ones). Everything afterwards travels in
//! ChaCha20-Poly1305 frames with separate keys per direction. Before anything is written, the
//! receiver sees which computer is sending what and must accept it; every file is checked
//! against the snapshot's SHA-256 list as it arrives, and an interrupted transfer resumes where
//! it stopped.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::build::{CHECKSUMS, Progress};
use crate::manifest::{MANIFEST_FILE, Manifest};
use crate::paths::{check_rel, join_rel, portable_component};
use crate::platform::{Device, Platform, free_space};
use crate::restore::ensure_dirs;
use crate::snapshot::parse_sums;
use crate::{Error, IoContext, Result, invalid};

const MAGIC: &[u8; 16] = b"SIMPLE-SETUP/1.0";
pub const SERVICE: &str = "_simple-setup._tcp.local.";
const CHUNK: usize = 256 * 1024;
const MAX_FRAME: usize = CHUNK + 4096;
/// Wrong pairing codes accepted before the receiver stops listening.
const MAX_ATTEMPTS: u32 = 5;
const MARKER: &str = ".simple-setup-receive.json";

/// A fresh 8-digit pairing code, shown as `1234-5678`.
pub fn new_code() -> String {
    // Rejection sampling keeps every code equally likely.
    let n = loop {
        let n: u32 = rand::random();
        if n < 4_200_000_000 {
            break n % 100_000_000;
        }
    };
    format!("{:04}-{:04}", n / 10_000, n % 10_000)
}

fn code_digits(code: &str) -> Result<String> {
    let d: String = code.chars().filter(char::is_ascii_digit).collect();
    if d.len() == 8 {
        Ok(d)
    } else {
        Err(invalid(
            "The pairing code has 8 digits, as shown on the other computer.",
        ))
    }
}

// ---------------------------------------------------------------- encrypted channel

struct Channel {
    stream: TcpStream,
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    sent: u64,
    got: u64,
}

fn lost(e: std::io::Error) -> Error {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            invalid("The other computer stopped responding.")
        }
        _ => invalid(format!("The connection was lost ({e}).")),
    }
}

fn nonce(n: u64) -> Nonce {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_be_bytes());
    *Nonce::from_slice(&b)
}

/// Raised when the other side used a different pairing code.
const WRONG_CODE: &str = "The pairing code does not match.";

impl Channel {
    fn open(mut stream: TcpStream, code: &str, sender: bool) -> Result<Self> {
        stream.write_all(MAGIC).map_err(lost)?;
        let mut magic = [0u8; 16];
        stream.read_exact(&mut magic).map_err(lost)?;
        if &magic != MAGIC {
            return Err(invalid(
                "The other side is not Simple Setup, or a different version.",
            ));
        }
        let (state, out) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(code_digits(code)?),
            &Identity::new(b"simple-setup snapshot transfer"),
        );
        stream
            .write_all(&(out.len() as u16).to_be_bytes())
            .map_err(lost)?;
        stream.write_all(&out).map_err(lost)?;
        let mut len = [0u8; 2];
        stream.read_exact(&mut len).map_err(lost)?;
        let len = u16::from_be_bytes(len) as usize;
        if len > 64 {
            return Err(invalid(WRONG_CODE));
        }
        let mut msg = vec![0u8; len];
        stream.read_exact(&mut msg).map_err(lost)?;
        let key = state.finish(&msg).map_err(|_| invalid(WRONG_CODE))?;
        let hk = Hkdf::<Sha256>::new(None, &key);
        let (mut s2r, mut r2s) = ([0u8; 32], [0u8; 32]);
        hk.expand(b"simple-setup sender to receiver", &mut s2r)
            .and_then(|()| hk.expand(b"simple-setup receiver to sender", &mut r2s))
            .map_err(|_| invalid("Key derivation failed."))?;
        let (send, recv) = if sender { (s2r, r2s) } else { (r2s, s2r) };
        Ok(Self {
            stream,
            send: ChaCha20Poly1305::new(Key::from_slice(&send)),
            recv: ChaCha20Poly1305::new(Key::from_slice(&recv)),
            sent: 0,
            got: 0,
        })
    }

    fn write_frame(&mut self, kind: u8, body: &[u8]) -> Result<()> {
        let mut plain = Vec::with_capacity(body.len() + 1);
        plain.push(kind);
        plain.extend_from_slice(body);
        let ct = self
            .send
            .encrypt(&nonce(self.sent), plain.as_slice())
            .map_err(|_| invalid("Encryption failed."))?;
        self.sent += 1;
        self.stream
            .write_all(&(ct.len() as u32).to_be_bytes())
            .map_err(lost)?;
        self.stream.write_all(&ct).map_err(lost)
    }

    fn read_frame(&mut self) -> Result<(u8, Vec<u8>)> {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).map_err(lost)?;
        let n = u32::from_be_bytes(len) as usize;
        if !(17..=MAX_FRAME + 17).contains(&n) {
            return Err(invalid("The other computer sent damaged data."));
        }
        let mut ct = vec![0u8; n];
        self.stream.read_exact(&mut ct).map_err(lost)?;
        let plain = self
            .recv
            .decrypt(&nonce(self.got), ct.as_slice())
            .map_err(|_| {
                invalid(if self.got == 0 {
                    WRONG_CODE
                } else {
                    "The connection was tampered with."
                })
            })?;
        self.got += 1;
        Ok((plain[0], plain[1..].to_vec()))
    }

    fn send_msg(&mut self, m: &Msg) -> Result<()> {
        let body = serde_json::to_vec(m).map_err(|e| invalid(e.to_string()))?;
        self.write_frame(0, &body)
    }

    fn recv_msg(&mut self) -> Result<Msg> {
        match self.read_frame()? {
            (0, body) => serde_json::from_slice(&body)
                .map_err(|e| invalid(format!("Unexpected message from the other computer: {e}"))),
            _ => Err(invalid("Unexpected data from the other computer.")),
        }
    }
}

// ---------------------------------------------------------------- messages

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OfferFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// What the sender proposes: shown to the receiver before anything is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub source: Device,
    pub targets: Vec<Platform>,
    pub encrypted: bool,
    pub files: Vec<OfferFile>,
}

impl Offer {
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Paths, names and checksums are untrusted until checked here.
    fn check(&self) -> Result<()> {
        if self.name.is_empty() || self.name.starts_with('.') || !portable_component(&self.name) {
            return Err(invalid(format!("Unusable snapshot name {:?}.", self.name)));
        }
        let mut seen = std::collections::HashSet::new();
        for f in &self.files {
            check_rel(&f.path, true)?;
            let hex = f.sha256.len() == 64 && f.sha256.bytes().all(|b| b.is_ascii_hexdigit());
            if !hex || !seen.insert(f.path.to_lowercase()) || f.path == MARKER {
                return Err(invalid(format!(
                    "Bad file entry {:?} in the offer.",
                    f.path
                )));
            }
        }
        if !seen.contains(&MANIFEST_FILE.to_lowercase())
            || !seen.contains(&CHECKSUMS.to_lowercase())
        {
            return Err(invalid("The offer is not a complete snapshot."));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Msg {
    Hello {
        device: Device,
    },
    Offer {
        offer: Offer,
    },
    /// Bytes the receiver already holds of each offered file (resume).
    Accept {
        have: Vec<u64>,
    },
    Decline {
        reason: String,
    },
    /// The data frames of file `index`, starting at `offset`, follow.
    File {
        index: usize,
        offset: u64,
    },
    Done,
    Received {
        path: String,
    },
    Failed {
        reason: String,
    },
}

/// The snapshot's files as listed in its checksum list, plus the list itself.
pub fn offer_for(bundle: &Path) -> Result<Offer> {
    let m = Manifest::read(bundle)?;
    let sums_path = bundle.join(CHECKSUMS);
    let text = fs::read_to_string(&sums_path).ctx(|| format!("Reading {}", sums_path.display()))?;
    let mut files = Vec::new();
    for (path, sha256) in parse_sums(&text)? {
        let size = fs::metadata(join_rel(bundle, &path, true)?)
            .ctx(|| format!("Reading {path}"))?
            .len();
        files.push(OfferFile {
            path,
            size,
            sha256: sha256.to_ascii_lowercase(),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.push(OfferFile {
        path: CHECKSUMS.into(),
        size: text.len() as u64,
        sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
    });
    Ok(Offer {
        id: m.id,
        name: m.name,
        created_at: m.created_at,
        source: m.source,
        targets: m.targets,
        encrypted: m.encryption.is_some(),
        files,
    })
}

// ---------------------------------------------------------------- sending

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SendReport {
    pub receiver: Device,
    pub files: u64,
    pub bytes: u64,
    /// Bytes the receiver already had from an interrupted transfer.
    pub resumed_bytes: u64,
    /// Where the receiver saved the snapshot.
    pub saved_as: String,
}

/// `host:port` or `ip:port` as typed by the user.
pub fn parse_address(s: &str) -> Result<SocketAddr> {
    s.trim()
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| {
            invalid(format!(
                "Not an address and port: {s:?} (for example 192.168.1.20:41234)."
            ))
        })
}

/// Sends `bundle` to a receiving computer. `confirm` sees the receiver before anything is sent.
pub fn send(
    bundle: &Path,
    to: SocketAddr,
    code: &str,
    confirm: &mut dyn FnMut(&Device) -> bool,
    emit: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<SendReport> {
    code_digits(code)?;
    let offer = offer_for(bundle)?;
    emit(Progress::Stage {
        label: format!("Connecting to {to}"),
        step: 1,
        steps: 2,
    });
    let stream = TcpStream::connect_timeout(&to, Duration::from_secs(10)).map_err(|e| {
        invalid(format!(
            "Could not reach {to} ({e}). Is the other computer still receiving?"
        ))
    })?;
    // Long enough for the person at the other computer to read the offer and accept it.
    stream
        .set_read_timeout(Some(Duration::from_secs(900)))
        .map_err(lost)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(120)))
        .map_err(lost)?;
    let mut ch = Channel::open(stream, code, true)?;
    ch.send_msg(&Msg::Hello {
        device: Device::detect()?,
    })?;
    // A receiver that cannot read our first message hangs up without a word.
    let receiver = match ch.recv_msg() {
        Ok(Msg::Hello { device }) => device,
        Ok(_) => return Err(invalid("Unexpected answer from the other computer.")),
        Err(_) => {
            return Err(invalid(
                "The other computer did not accept the pairing code. Check the code it shows and try again.",
            ));
        }
    };
    if !confirm(&receiver) {
        let _ = ch.send_msg(&Msg::Decline {
            reason: "The sender cancelled.".into(),
        });
        return Err(Error::Cancelled);
    }
    ch.send_msg(&Msg::Offer {
        offer: offer.clone(),
    })?;
    let have = match ch.recv_msg()? {
        Msg::Accept { have } if have.len() == offer.files.len() => have,
        Msg::Decline { reason } => {
            return Err(invalid(format!("{} declined: {reason}", receiver.hostname)));
        }
        _ => return Err(invalid("Unexpected answer from the other computer.")),
    };
    let total = offer.bytes();
    let resumed: u64 = offer
        .files
        .iter()
        .zip(&have)
        .map(|(f, h)| (*h).min(f.size))
        .sum();
    let mut done = resumed;
    emit(Progress::Stage {
        label: format!("Sending to {}", receiver.hostname),
        step: 2,
        steps: 2,
    });
    let mut buf = vec![0u8; CHUNK];
    for (index, (f, &h)) in offer.files.iter().zip(&have).enumerate() {
        let offset = h.min(f.size);
        if offset == f.size {
            continue;
        }
        ch.send_msg(&Msg::File { index, offset })?;
        let mut file =
            File::open(join_rel(bundle, &f.path, true)?).ctx(|| format!("Reading {}", f.path))?;
        file.seek(SeekFrom::Start(offset))
            .ctx(|| format!("Reading {}", f.path))?;
        let mut left = f.size - offset;
        while left > 0 {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let want = buf.len().min(left as usize);
            file.read_exact(&mut buf[..want])
                .ctx(|| format!("Reading {} (did it change?)", f.path))?;
            ch.write_frame(1, &buf[..want])?;
            left -= want as u64;
            done += want as u64;
            emit(Progress::Copy {
                done_bytes: done,
                total_bytes: total,
                done_files: index as u64,
                total_files: offer.files.len() as u64,
                current: f.path.clone(),
            });
        }
    }
    emit(Progress::Copy {
        done_bytes: total,
        total_bytes: total,
        done_files: offer.files.len() as u64,
        total_files: offer.files.len() as u64,
        current: String::new(),
    });
    ch.send_msg(&Msg::Done)?;
    match ch.recv_msg()? {
        Msg::Received { path } => Ok(SendReport {
            receiver,
            files: offer.files.len() as u64,
            bytes: total,
            resumed_bytes: resumed,
            saved_as: path,
        }),
        Msg::Failed { reason } => Err(invalid(format!(
            "The other computer could not save the snapshot: {reason}"
        ))),
        _ => Err(invalid("Unexpected answer from the other computer.")),
    }
}

// ---------------------------------------------------------------- discovering

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Peer {
    pub name: String,
    pub os: Option<String>,
    pub addresses: Vec<SocketAddr>,
}

/// Computers on this network that are receiving right now (listens for `wait`).
pub fn discover(wait: Duration) -> Result<Vec<Peer>> {
    let d = ServiceDaemon::new()
        .map_err(|e| invalid(format!("Looking for nearby computers failed: {e}")))?;
    let rx = d
        .browse(SERVICE)
        .map_err(|e| invalid(format!("Looking for nearby computers failed: {e}")))?;
    let end = Instant::now() + wait;
    let mut peers: Vec<Peer> = Vec::new();
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(s)) => {
                let name = s
                    .get_fullname()
                    .trim_end_matches(SERVICE)
                    .trim_end_matches('.')
                    .to_string();
                let mut addresses: Vec<SocketAddr> = s
                    .get_addresses()
                    .iter()
                    .map(|ip| ip.to_ip_addr())
                    .filter(|ip| match ip {
                        IpAddr::V4(_) => true,
                        // Link-local IPv6 needs an interface scope that plain sockets lose.
                        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
                    })
                    .map(|ip| SocketAddr::new(ip, s.get_port()))
                    .collect();
                addresses.sort_by_key(|a| a.is_ipv6());
                let os = s
                    .txt_properties
                    .get_property_val_str("os")
                    .map(String::from);
                peers.retain(|p| p.name != name);
                if !addresses.is_empty() {
                    peers.push(Peer {
                        name,
                        os,
                        addresses,
                    });
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = d.shutdown();
    peers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(peers)
}

// ---------------------------------------------------------------- receiving

/// Receiving mode: a listening socket, an mDNS announcement and a pairing code. Everything
/// stops when this is dropped.
pub struct Receiver {
    listener: TcpListener,
    pub code: String,
    pub port: u16,
    /// Why the computer is not announced on the network, if it is not.
    pub announce_error: Option<String>,
    mdns: Option<(ServiceDaemon, String)>,
}

/// This computer's address on the local network, for typing in by hand.
pub fn local_address() -> Option<IpAddr> {
    // Connecting a UDP socket sends nothing; it only picks the outgoing interface.
    let s = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    s.connect(("192.0.2.1", 9)).ok()?;
    s.local_addr()
        .ok()
        .map(|a| a.ip())
        .filter(|ip| !ip.is_unspecified())
}

impl Receiver {
    /// Starts listening (on all interfaces, a random port) and, if `announce`, announces this
    /// computer over mDNS until dropped.
    pub fn start(announce: bool) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", 0)).ctx(|| "Starting to receive".into())?;
        listener
            .set_nonblocking(true)
            .ctx(|| "Starting to receive".into())?;
        let port = listener
            .local_addr()
            .ctx(|| "Starting to receive".into())?
            .port();
        let mut r = Self {
            listener,
            code: new_code(),
            port,
            announce_error: None,
            mdns: None,
        };
        if announce {
            match announce_self(port) {
                Ok(m) => r.mdns = Some(m),
                Err(e) => r.announce_error = Some(e.to_string()),
            }
        }
        Ok(r)
    }

    /// Waits for a sender, receives one snapshot into `dest/<name>` and returns that folder.
    /// Interrupted transfers wait for the sender to reconnect and resume. `accept` decides
    /// whether to take an offer (asked once per snapshot).
    pub fn run(
        &self,
        dest: &Path,
        accept: &mut dyn FnMut(&Device, &Offer) -> bool,
        emit: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let mut wrong = 0;
        let mut accepted: Option<String> = None;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let stream = match self.listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => return Err(lost(e)),
            };
            let setup = stream
                .set_nonblocking(false)
                .and_then(|()| stream.set_read_timeout(Some(Duration::from_secs(120))))
                .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(120))));
            if setup.is_err() {
                continue;
            }
            let mut paired = false;
            match self.handle(
                stream,
                dest,
                &mut accepted,
                &mut paired,
                accept,
                emit,
                cancel,
            ) {
                Ok(p) => return Ok(p),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) if !paired => {
                    wrong += 1;
                    if wrong >= MAX_ATTEMPTS {
                        return Err(invalid(
                            "Too many attempts with a wrong pairing code. Receiving stopped; start again for a new code.",
                        ));
                    }
                    emit(Progress::Note {
                        message: format!("A computer could not pair ({e})"),
                    });
                }
                Err(e) if e.to_string().starts_with("You declined") => return Err(e),
                Err(e) => emit(Progress::Note {
                    message: format!(
                        "The transfer stopped ({e}). Waiting for the sender to try again; it will continue where it stopped."
                    ),
                }),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle(
        &self,
        stream: TcpStream,
        dest: &Path,
        accepted: &mut Option<String>,
        paired: &mut bool,
        accept: &mut dyn FnMut(&Device, &Offer) -> bool,
        emit: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let mut ch = Channel::open(stream, &self.code, false)?;
        let sender = match ch.recv_msg()? {
            Msg::Hello { device } => device,
            _ => return Err(invalid("Unexpected message from the sender.")),
        };
        *paired = true;
        ch.send_msg(&Msg::Hello {
            device: Device::detect()?,
        })?;
        let offer = match ch.recv_msg()? {
            Msg::Offer { offer } => offer,
            Msg::Decline { reason } => {
                return Err(invalid(format!("The sender stopped: {reason}")));
            }
            _ => return Err(invalid("Unexpected message from the sender.")),
        };
        if let Err(e) = offer.check() {
            let _ = ch.send_msg(&Msg::Decline {
                reason: e.to_string(),
            });
            return Err(e);
        }
        let result = receive_offer(
            &mut ch, &sender, &offer, dest, accepted, accept, emit, cancel,
        );
        if let Err(e) = &result
            && !matches!(e, Error::Cancelled)
        {
            let _ = ch.send_msg(&Msg::Failed {
                reason: e.to_string(),
            });
        }
        result
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if let Some((d, name)) = &self.mdns {
            let _ = d.unregister(name);
            let _ = d.shutdown();
        }
    }
}

fn announce_self(port: u16) -> Result<(ServiceDaemon, String)> {
    let me = Device::detect()?;
    let host: String = me
        .hostname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let host = if host.is_empty() {
        "simple-setup".to_string()
    } else {
        host
    };
    let d = ServiceDaemon::new()
        .map_err(|e| invalid(format!("Announcing on the network failed: {e}")))?;
    let os = me.platform.to_string();
    let info = ServiceInfo::new(
        SERVICE,
        &host,
        &format!("{host}.local."),
        "",
        port,
        &[("v", "1"), ("os", os.as_str())][..],
    )
    .map_err(|e| invalid(format!("Announcing on the network failed: {e}")))?
    .enable_addr_auto();
    let name = info.get_fullname().to_string();
    d.register(info)
        .map_err(|e| invalid(format!("Announcing on the network failed: {e}")))?;
    Ok((d, name))
}

#[allow(clippy::too_many_arguments)]
fn receive_offer(
    ch: &mut Channel,
    sender: &Device,
    offer: &Offer,
    dest: &Path,
    accepted: &mut Option<String>,
    accept: &mut dyn FnMut(&Device, &Offer) -> bool,
    emit: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let final_dir = dest.join(&offer.name);
    let staging = dest.join(format!(".{}.receiving", offer.name));
    let already = final_dir.exists();
    if already
        && Manifest::read(&final_dir).map(|m| m.id).ok().as_deref() != Some(offer.id.as_str())
    {
        let why = format!(
            "A different snapshot named \"{}\" is already in {}.",
            offer.name,
            dest.display()
        );
        let _ = ch.send_msg(&Msg::Decline {
            reason: why.clone(),
        });
        return Err(invalid(why));
    }
    if accepted.as_deref() != Some(offer.id.as_str()) {
        if !accept(sender, offer) {
            let _ = ch.send_msg(&Msg::Decline {
                reason: "The person at this computer declined.".into(),
            });
            return Err(invalid("You declined the snapshot."));
        }
        *accepted = Some(offer.id.clone());
    }
    if already {
        ch.send_msg(&Msg::Accept {
            have: offer.files.iter().map(|f| f.size).collect(),
        })?;
        if !matches!(ch.recv_msg()?, Msg::Done) {
            return Err(invalid("Unexpected message from the sender."));
        }
        ch.send_msg(&Msg::Received {
            path: final_dir.display().to_string(),
        })?;
        return Ok(final_dir);
    }
    prepare_staging(&staging, &offer.id)?;
    let have = held(&staging, offer, cancel)?;
    let needed: u64 = offer.files.iter().zip(&have).map(|(f, h)| f.size - h).sum();
    if let Some(free) = free_space(dest)
        && free < needed + (16 << 20)
    {
        let why = format!(
            "Not enough space: {needed} bytes needed, {free} free in {}.",
            dest.display()
        );
        let _ = ch.send_msg(&Msg::Decline {
            reason: why.clone(),
        });
        return Err(invalid(why));
    }
    ch.send_msg(&Msg::Accept { have: have.clone() })?;
    let total = offer.bytes();
    let mut done: u64 = have.iter().sum();
    emit(Progress::Stage {
        label: format!("Receiving \"{}\" from {}", offer.name, sender.hostname),
        step: 1,
        steps: 1,
    });
    let mut complete: Vec<bool> = offer
        .files
        .iter()
        .zip(&have)
        .map(|(f, h)| *h == f.size)
        .collect();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        match ch.recv_msg()? {
            Msg::File { index, offset } => {
                let f = offer
                    .files
                    .get(index)
                    .ok_or_else(|| invalid("Unknown file from the sender."))?;
                if offset != have[index] || complete[index] {
                    return Err(invalid("The sender resumed at the wrong place."));
                }
                let path = join_rel(&staging, &f.path, true)?;
                ensure_dirs(&staging, path.parent().unwrap_or(&staging))?;
                let mut hasher = Sha256::new();
                let mut out = if offset == 0 {
                    File::create(&path).ctx(|| format!("Writing {}", path.display()))?
                } else {
                    let mut prev =
                        File::open(&path).ctx(|| format!("Reading {}", path.display()))?;
                    std::io::copy(&mut (&mut prev).take(offset), &mut HashWriter(&mut hasher))
                        .ctx(|| format!("Reading {}", path.display()))?;
                    OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .ctx(|| format!("Writing {}", path.display()))?
                };
                let mut left = f.size - offset;
                while left > 0 {
                    let (kind, data) = ch.read_frame()?;
                    if kind != 1 || data.len() as u64 > left {
                        return Err(invalid("The sender sent more data than announced."));
                    }
                    out.write_all(&data)
                        .ctx(|| format!("Writing {} (is the disk full?)", path.display()))?;
                    hasher.update(&data);
                    left -= data.len() as u64;
                    done += data.len() as u64;
                    emit(Progress::Copy {
                        done_bytes: done,
                        total_bytes: total,
                        done_files: index as u64,
                        total_files: offer.files.len() as u64,
                        current: f.path.clone(),
                    });
                }
                out.sync_all()
                    .ctx(|| format!("Writing {}", path.display()))?;
                if format!("{:x}", hasher.finalize()) != f.sha256 {
                    let _ = fs::remove_file(&path);
                    return Err(invalid(format!(
                        "{} arrived damaged and was discarded.",
                        f.path
                    )));
                }
                complete[index] = true;
            }
            Msg::Done => break,
            Msg::Decline { reason } | Msg::Failed { reason } => {
                return Err(invalid(format!("The sender stopped: {reason}")));
            }
            _ => return Err(invalid("Unexpected message from the sender.")),
        }
    }
    if let Some(i) = complete.iter().position(|c| !c) {
        return Err(invalid(format!("{} is missing.", offer.files[i].path)));
    }
    emit(Progress::Copy {
        done_bytes: total,
        total_bytes: total,
        done_files: offer.files.len() as u64,
        total_files: offer.files.len() as u64,
        current: String::new(),
    });
    // The snapshot's own checksum list must list exactly the offered files.
    let sums = parse_sums(
        &fs::read_to_string(staging.join(CHECKSUMS)).ctx(|| "Reading the checksum list".into())?,
    )?;
    let listed = offer.files.iter().filter(|f| f.path != CHECKSUMS);
    if sums.len() != offer.files.len() - 1
        || listed.into_iter().any(|f| {
            sums.get(&f.path)
                .is_none_or(|h| !h.eq_ignore_ascii_case(&f.sha256))
        })
    {
        return Err(invalid(
            "The snapshot's checksum list does not match what was sent.",
        ));
    }
    Manifest::read(&staging)?;
    let _ = fs::remove_file(staging.join(MARKER));
    fs::rename(&staging, &final_dir).ctx(|| format!("Saving {}", final_dir.display()))?;
    ch.send_msg(&Msg::Received {
        path: final_dir.display().to_string(),
    })?;
    Ok(final_dir)
}

/// A staging folder for this snapshot, reused (resume) only if it belongs to the same one.
fn prepare_staging(staging: &Path, id: &str) -> Result<()> {
    let marker = staging.join(MARKER);
    if staging.exists() {
        match fs::read_to_string(&marker) {
            Ok(m) if m.trim() == id => return Ok(()),
            Ok(_) => {
                fs::remove_dir_all(staging).ctx(|| format!("Clearing {}", staging.display()))?
            }
            Err(_) => {
                return Err(invalid(format!(
                    "{} exists but was not made by Simple Setup; move it away first.",
                    staging.display()
                )));
            }
        }
    }
    fs::create_dir_all(staging).ctx(|| format!("Creating {}", staging.display()))?;
    fs::write(&marker, id).ctx(|| format!("Writing {}", marker.display()))
}

/// Bytes already held of each offered file. Complete files are re-checked; damaged or
/// oversized ones start over.
fn held(staging: &Path, offer: &Offer, cancel: &AtomicBool) -> Result<Vec<u64>> {
    let mut have = Vec::with_capacity(offer.files.len());
    for f in &offer.files {
        let p = join_rel(staging, &f.path, true)?;
        let size = fs::symlink_metadata(&p)
            .ok()
            .filter(|m| m.is_file())
            .map_or(0, |m| m.len());
        let keep = match size {
            0 => 0,
            s if s > f.size => 0,
            s if s == f.size => {
                let ok =
                    crate::build::hash_file(&p, cancel, |_| {}).is_ok_and(|(_, h)| h == f.sha256);
                if ok { s } else { 0 }
            }
            s => s,
        };
        if keep == 0 && size > 0 {
            let _ = fs::remove_file(&p);
        }
        have.push(keep);
    }
    Ok(have)
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_codes() {
        let c = new_code();
        assert_eq!(c.len(), 9);
        assert_eq!(code_digits(&c).unwrap().len(), 8);
        assert_eq!(code_digits(" 1234 5678 ").unwrap(), "12345678");
        assert!(code_digits("1234").is_err());
    }

    #[test]
    fn hostile_offers_are_refused() {
        let ok = |path: &str| OfferFile {
            path: path.into(),
            size: 1,
            sha256: "a".repeat(64),
        };
        let mut o = Offer {
            id: "x".into(),
            name: "Move".into(),
            created_at: String::new(),
            source: Device {
                hostname: "a".into(),
                os_name: "b".into(),
                platform: Platform::current().unwrap(),
            },
            targets: vec![],
            encrypted: false,
            files: vec![
                ok(MANIFEST_FILE),
                ok(CHECKSUMS),
                ok("personal-files/Documents/a.txt"),
            ],
        };
        assert!(o.check().is_ok());
        for bad in ["../outside", "/etc/passwd", "a/../../b", MARKER] {
            o.files[2] = ok(bad);
            assert!(o.check().is_err(), "{bad}");
        }
        o.files[2] = ok("x");
        o.name = "..".into();
        assert!(o.check().is_err());
    }

    /// Two ends with different codes derive different keys; the first frame fails to open.
    #[test]
    fn wrong_code_cannot_read() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut ch = Channel::open(s, "1111-1111", false).unwrap();
            ch.recv_msg().map(|_| ()).unwrap_err().to_string()
        });
        let mut ch = Channel::open(TcpStream::connect(addr).unwrap(), "2222-2222", true).unwrap();
        ch.send_msg(&Msg::Done).unwrap();
        assert_eq!(t.join().unwrap(), WRONG_CODE);
    }
}
