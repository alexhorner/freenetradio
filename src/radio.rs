//! Internet radio: the stream client (HTTP/HTTPS + ICY metadata), the
//! shared player status, and the MP3 decode -> I2S output task.
//!
//! Data flow:
//!   stream_task --(MP3 bytes)--> STREAM pipe --> audio_task --(PCM)--> I2S ring
//!
//! The pipe plus the TCP receive buffer (~1.5 s at 128 kbit/s together)
//! absorb network jitter. When it's full
//! the stream task simply stops reading, which back-pressures the TCP
//! connection.

use core::cell::RefCell;
use core::fmt::Write as _;

use critical_section::Mutex;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_futures::yield_now;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pipe::Pipe;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use crate::ElapsedSat;
use embedded_tls::{Aes128GcmSha256, MaxFragmentLength, TlsConfig, TlsConnection, TlsContext, TlsError, UnsecureProvider};
use hal::peripherals::RNG;
use hal::rng::Rng;
use nanomp3_core::{Channels, Decoder, MAX_SAMPLES_PER_FRAME};

use crate::hal;
use crate::i2s::I2sOut;
use crate::text::Text;

/// Compressed-stream buffer between the network and the decoder (the TCP
/// receive buffer below adds another 10 KB in front of it).
pub const PIPE_LEN: usize = 10 * 1024; // RAM is tight: the stack needs the rest
/// Start (or resume) playback once the pipe is (nearly) full and has stayed
/// so for PREBUFFER_HOLD, so the TCP receive buffer behind it fills too.
/// Starting with every buffer full matters at high bitrates: the buffers
/// are sized in bytes, ~0.5 s in all at 320 kbit/s, and some servers send
/// in ~16 KB lumps with pauses of up to ~0.5 s between them (faster than
/// real time on average, so once full the buffers stay full). Starting at
/// 3/4 of the pipe (~0.27 s at 320k) underran in the first 2 s about half
/// the time.
const PREBUFFER: usize = PIPE_LEN * 15 / 16;
const PREBUFFER_HOLD: Duration = Duration::from_millis(400);
/// Decoder input window. The decoder needs a couple of frames beyond the
/// current one to recognise it (an MP3 frame is ~420 bytes at 128 kbit/s,
/// ~1 KB at 320 kbit/s).
pub const WINDOW: usize = 4 * 1024;
const MIN_DECODE_INPUT: usize = 2560;

pub static STREAM: Pipe<CriticalSectionRawMutex, PIPE_LEN> = Pipe::new();
/// Lowest number of frames queued for the DAC when the audio task checked,
/// and the longest gap between its passes (diagnostics; reset when read).
#[cfg(feature = "diag")]
pub static MIN_QUEUED: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(u32::MAX);
/// Frames the decoder failed to produce (each is a gap in the audio):
/// [no frame, reservoir unavailable, corrupt, other].
#[cfg(feature = "diag")]
pub static DECODE_ERRORS: [portable_atomic::AtomicU32; 4] = [const { portable_atomic::AtomicU32::new(0) }; 4];
#[cfg(feature = "diag")]
pub static MAX_LOOP_GAP_US: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);

/// Audio task loop iterations (diagnostics: shows it's alive).
#[cfg(feature = "diag")]
pub static AUDIO_LOOPS: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
/// Total stream bytes received (diagnostics).
#[cfg(feature = "diag")]
pub static STREAM_BYTES: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
/// How long the last TLS handshake took (diagnostics).
/// Diagnostics: how far the last connection got (1 DNS, 2 TCP connect,
/// 3 TLS handshake, 4 request sent, 5 headers received, 6 streaming).
pub static CONNECT_STAGE: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
fn stage(n: u32) {
    CONNECT_STAGE.store(n, portable_atomic::Ordering::Relaxed);
}
/// Diagnostics at the last TLS read error: TCP recv queue, TCP send queue,
/// TLS reader decoded and pending bytes.
#[cfg(feature = "diag")]
pub static FAIL_INFO: Mutex<RefCell<[u32; 4]>> = Mutex::new(RefCell::new([0; 4]));
/// Diagnostics for the last underrun: [ms since playback started,
/// 1 = DMA overtook the decoder / 2 = ran out of stream data, compressed
/// bytes buffered, frames queued for the DAC].
#[cfg(feature = "diag")]
pub static UNDERRUN_INFO: Mutex<RefCell<[u32; 4]>> = Mutex::new(RefCell::new([0; 4]));
#[cfg(feature = "diag")]
pub static TLS_READ_MAX_US: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
#[cfg(feature = "diag")]
pub static TLS_HANDSHAKE_MS: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Connecting,
    Buffering,
    Playing,
    Error,
    /// The network link is down (or has no address yet); the stream picks
    /// up again by itself when it's back.
    Offline,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Stopped => "stopped",
            Phase::Connecting => "connecting",
            Phase::Buffering => "buffering",
            Phase::Playing => "playing",
            Phase::Error => "error",
            Phase::Offline => "offline",
        }
    }
}

/// Everything the web page and display show about the player.
#[derive(Clone, Copy)]
pub struct Status {
    pub phase: Phase,
    /// Error or end-of-stream explanation.
    pub message: Text<96>,
    pub url: Text<256>,
    /// icy-name header (station name, usually with a description).
    pub station: Text<128>,
    /// Latest StreamTitle from the ICY metadata.
    pub title: Text<128>,
    pub sample_rate: u32,
    pub bitrate: u32,
    pub channels: u8,
    /// Compressed data buffered, 0..=100 %.
    pub buffer_pct: u8,
    /// Times playback ran dry (since power-on).
    pub underruns: u32,
    /// 0..=100.
    pub volume: u8,
    /// Mute (the remote's * button); the volume setting is kept.
    pub muted: bool,
    /// Peak levels of the latest decoded frame, 0..=255.
    pub level_l: u8,
    pub level_r: u8,
    /// Bumped on every play/stop so the audio task can reset.
    pub generation: u32,
    /// Decode time per MP3 frame, microseconds (max since last read, average).
    pub decode_max_us: u32,
    pub decode_avg_us: u32,
}

impl Status {
    const fn new() -> Self {
        Self {
            phase: Phase::Stopped,
            message: Text::new(),
            url: Text::new(),
            station: Text::new(),
            title: Text::new(),
            sample_rate: 0,
            bitrate: 0,
            channels: 0,
            buffer_pct: 0,
            underruns: 0,
            volume: 10,
            muted: false,
            level_l: 0,
            level_r: 0,
            generation: 0,
            decode_max_us: 0,
            decode_avg_us: 0,
        }
    }
}

static STATUS: Mutex<RefCell<Status>> = Mutex::new(RefCell::new(Status::new()));

#[derive(Clone, Copy)]
enum Command {
    Play,
    Stop,
}

static COMMAND: Signal<CriticalSectionRawMutex, Command> = Signal::new();

fn with_status<R>(f: impl FnOnce(&mut Status) -> R) -> R {
    critical_section::with(|cs| f(&mut STATUS.borrow_ref_mut(cs)))
}

pub fn status() -> Status {
    critical_section::with(|cs| *STATUS.borrow_ref(cs))
}

/// Start playing `url` (replacing whatever is playing).
pub fn play(url: &str) {
    with_status(|s| s.url = Text::from_str(url.trim()));
    COMMAND.signal(Command::Play);
}

pub fn stop() {
    COMMAND.signal(Command::Stop);
}

/// Bring back saved settings at startup (settings.rs): the station (played
/// if `play`, otherwise just selected so OK plays it), volume and mute.
pub fn restore(url: &str, volume: u8, muted: bool, play: bool) {
    with_status(|s| {
        s.url = Text::from_str(url);
        s.volume = volume.min(100);
        s.muted = muted || volume == 0;
    });
    if play && !url.is_empty() {
        COMMAND.signal(Command::Play);
    }
}

/// Volume after unmuting from 0.
const UNMUTE_VOLUME: u8 = 2;

/// Set the volume, 0..=100. 0 means muted; anything else unmutes.
pub fn set_volume(volume: u8) {
    with_status(|s| {
        s.volume = volume.min(100);
        s.muted = s.volume == 0;
    });
}

/// Toggle mute; returns whether it's now muted. Unmuting at volume 0 goes
/// to a quiet UNMUTE_VOLUME rather than staying silent.
pub fn toggle_mute() -> bool {
    with_status(|s| {
        s.muted = !s.muted;
        if !s.muted && s.volume == 0 {
            s.volume = UNMUTE_VOLUME;
        }
        s.muted
    })
}

/// Read and reset the max decode time (so /stats shows recent peaks).
#[cfg(feature = "diag")]
pub fn take_decode_max_us() -> u32 {
    with_status(|s| core::mem::take(&mut s.decode_max_us))
}

fn set_phase(phase: Phase, message: &str) {
    with_status(|s| {
        s.phase = phase;
        s.message = Text::from_str(message);
        if !matches!(phase, Phase::Buffering | Phase::Playing) {
            s.level_l = 0;
            s.level_r = 0;
            s.buffer_pct = 0;
        }
    });
}

// ---------------------------------------------------------------------------
// Stream client
// ---------------------------------------------------------------------------

/// Network buffer for the stream connection. Plain http uses all of it as
/// the TCP receive buffer. https splits it: TLS record buffers at the end,
/// a smaller TCP receive buffer in front.
///
/// The TCP receive window matters: throughput is at most window / round
/// trip time, and internet stations can be 100-300 ms away. With 4 KB, a
/// 128 kbit/s (16 KB/s) stream arrived at only ~10 KB/s. 10 KB (7 KB with
/// TLS) still carries 320 kbit/s (40 KB/s) from ~175 ms away; it was 12 KB
/// until 2 KB went to a 7th Ethernet RX buffer (see main.rs).
const NET_LEN: usize = 10 * 1024;
/// TLS record read buffer: one whole encrypted record. A record may be up to
/// 16 KB + 256 by default, more RAM than there is to spare, so the client
/// asks for 2 KB fragments (max_fragment_length; honoured by the Icecast
/// servers tried, which run OpenSSL) and records are then at most
/// 2048 + 256 + 5 bytes. Small records also keep each decrypt short: a
/// record is decrypted in one go (~2.2 us/byte, no await inside), and 4 KB
/// records (~9 ms) on top of an MP3 frame decode (~12 ms) kept the network
/// task away long enough for Ethernet RX buffers to overflow at 320 kbit/s.
/// Servers that ignore the request (e.g. BoringSSL/Cloudflare) fail as soon
/// as they send a bigger record.
const TLS_READ: usize = 2560;
/// TLS record write buffer: just the HTTP request (split into several
/// records if it's longer).
const TLS_WRITE: usize = 512;
const _: () = assert!(TLS_READ >= 2048 + 256 + 5);

/// Fetches the stream and feeds the MP3 bytes into `STREAM`.
#[embassy_executor::task]
pub async fn stream_task(stack: Stack<'static>, mut rng: Rng<'static, RNG>) -> ! {
    let mut net = [0u8; NET_LEN];
    let mut tx = [0u8; 768];
    let mut pending: Option<Command> = None;
    loop {
        let command = match pending.take() {
            Some(c) => c,
            None => COMMAND.wait().await,
        };
        // Whatever happens next, the old stream's data is no longer wanted.
        STREAM.clear();
        with_status(|s| {
            s.generation = s.generation.wrapping_add(1);
            s.station.clear();
            s.title.clear();
            s.sample_rate = 0;
            s.bitrate = 0;
            s.channels = 0;
        });
        match command {
            Command::Stop => set_phase(Phase::Stopped, ""),
            Command::Play => {
                set_phase(Phase::Connecting, "");
                let url: Text<256> = with_status(|s| s.url);
                // Failed starts in a row; reset whenever audio got flowing.
                let mut retries = 0;
                // Just back from a network outage: the switch, router or
                // DHCP may need a moment, so also retry the failures that
                // are normally final (DNS, connect), and more times.
                let mut after_outage = false;
                loop {
                    // No link, or no address yet: say so and wait.
                    if !stack.is_link_up() || !stack.is_config_up() {
                        if let Some(next) = wait_for_network(stack).await {
                            pending = Some(next);
                            break;
                        }
                        retries = 0;
                        after_outage = true;
                    }
                    let fetching = fetch(stack, url, &mut net, &mut tx, &mut rng);
                    match select3(fetching, COMMAND.wait(), stack.wait_link_down()).await {
                        Either3::First(Ok(())) => set_phase(Phase::Stopped, "The stream ended"),
                        // The link went down (or the fetch failed because it
                        // had): drop the connection, then wait for it above.
                        Either3::Third(()) => {
                            OUTAGES.fetch_add(1, portable_atomic::Ordering::Relaxed);
                            continue;
                        }
                        Either3::First(Err(_)) if !stack.is_link_up() => {
                            OUTAGES.fetch_add(1, portable_atomic::Ordering::Relaxed);
                            continue;
                        }
                        Either3::First(Err(fail))
                            if (fail.retry || after_outage) && retries < if after_outage { MAX_RETRIES_AFTER_OUTAGE } else { MAX_RETRIES } =>
                        {
                            if CONNECT_STAGE.load(portable_atomic::Ordering::Relaxed) == 6 {
                                retries = 0;
                                after_outage = false;
                            }
                            retries += 1;
                            RECONNECTS.fetch_add(1, portable_atomic::Ordering::Relaxed);
                            // Keep playing what's buffered (the phase stays
                            // Playing) while reconnecting.
                            with_status(|s| {
                                if !matches!(s.phase, Phase::Playing | Phase::Buffering) {
                                    s.phase = Phase::Connecting;
                                }
                                s.message = Text::from_str("Reconnecting...");
                            });
                            let pause = if after_outage { 2 } else { 1 };
                            match select(Timer::after_secs(pause), COMMAND.wait()).await {
                                Either::First(()) => continue,
                                Either::Second(next) => pending = Some(next),
                            }
                        }
                        Either3::First(Err(fail)) => set_phase(Phase::Error, fail.msg.as_str()),
                        Either3::Second(next) => pending = Some(next),
                    }
                    break;
                }
                STREAM.clear();
            }
        }
    }
}

type Msg = Text<96>;

/// Why fetching a stream stopped, and whether reconnecting might help
/// (lost connections and servers that go quiet, as opposed to bad URLs,
/// HTTP errors and unsupported formats).
struct Fail {
    msg: Msg,
    retry: bool,
}

impl From<Msg> for Fail {
    fn from(msg: Msg) -> Self {
        Fail { msg, retry: false }
    }
}

fn retry(m: &str) -> Fail {
    Fail { msg: msg(m), retry: true }
}

/// Reconnect attempts before giving up on a stream that won't start.
/// A stream that was playing gets a fresh set each time it drops.
const MAX_RETRIES: u32 = 3;
/// Attempts after the network comes back (2 s apart), for any failure.
const MAX_RETRIES_AFTER_OUTAGE: u32 = 10;
/// Times the link went down under a stream (diagnostics; not counting
/// waiting for the network at boot).
pub static OUTAGES: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);

const LINK_DOWN: &str = "The network link is down. Please ensure the network cable is properly connected at both ends.";

/// Show that the network is gone and wait for the link and an address to
/// come back. Returns a command (play/stop) that came in meanwhile, or None
/// once the network is back.
async fn wait_for_network(stack: Stack<'static>) -> Option<Command> {
    STREAM.clear();
    let back = async {
        if !stack.is_link_up() {
            set_phase(Phase::Offline, LINK_DOWN);
            stack.wait_link_up().await;
        }
        // Link up but no address yet (also the normal case at boot, for a
        // moment, before DHCP has answered).
        set_phase(Phase::Offline, "Getting a network address...");
        stack.wait_config_up().await;
    };
    match select(back, COMMAND.wait()).await {
        Either::First(()) => {
            set_phase(Phase::Connecting, "Network back, reconnecting...");
            None
        }
        Either::Second(command) => Some(command),
    }
}
/// How long to wait for the response headers. Some https servers
/// occasionally accept the request and then send nothing at all; a new
/// connection works.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Reconnect if a stream that was flowing goes quiet this long. A live
/// stream never pauses like this: it means the connection is stuck, e.g.
/// after a network blip, when the server's TCP retries back off for many
/// seconds even once the network is back. Without this the radio waited
/// for the 15 s socket timeout, long after the buffer (~1.5 s) ran dry.
const STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Diagnostics: automatic reconnects since power-on.
pub static RECONNECTS: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);

fn msg(s: &str) -> Msg {
    Text::from_str(s)
}

struct Url<'a> {
    tls: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

fn parse_url(url: &str) -> Result<Url<'_>, Msg> {
    if url.is_empty() {
        return Err(msg("Enter a stream URL"));
    }
    let (tls, rest) = if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        return Err(msg("The URL must start with http:// or https://"));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| msg("Bad port number in the URL"))?),
        None => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(msg("The URL has no host name"));
    }
    Ok(Url { tls, host, port, path })
}

/// The stream connection: plain TCP, or TLS over TCP.
enum Conn<'a> {
    Plain(TcpSocket<'a>),
    Tls(TlsConnection<'a, TcpSocket<'a>, Aes128GcmSha256>),
}

impl Conn<'_> {
    /// Read some bytes; 0 at the end of the stream. `lost` is the error
    /// message for a broken connection.
    async fn read(&mut self, buf: &mut [u8], lost: &str) -> Result<usize, Msg> {
        match self {
            Conn::Plain(s) => s.read(buf).await.map_err(|_| msg(lost)),
            Conn::Tls(t) => match {
                #[cfg(feature = "diag")]
                let t0 = Instant::now();
                let r = t.read(buf).await;
                #[cfg(feature = "diag")]
                TLS_READ_MAX_US.fetch_max(t0.elapsed_sat().as_micros() as u32, portable_atomic::Ordering::Relaxed);
                r
            } {
                Ok(n) => {
                    // Decrypting a record takes a few ms, and when several
                    // are queued in the TCP buffer nothing here would yield
                    // between them (pipe writes don't wait while there's
                    // room), starving the audio task for ~100 ms. Let the
                    // other tasks run after each one.
                    yield_now().await;
                    Ok(n)
                }
                Err(e) => {
                    #[cfg(feature = "diag")]
                    TLS_LAST_ERROR.store(tls_error_code(&e), portable_atomic::Ordering::Relaxed);
                    #[cfg(feature = "diag")]
                    {
                        let sock = t.delegate();
                        let (decoded, pending) = t.reader_state();
                        critical_section::with(|cs| {
                            *FAIL_INFO.borrow_ref_mut(cs) = [sock.recv_queue() as u32, sock.send_queue() as u32, decoded as u32, pending as u32];
                        });
                    }
                    match e {
                        TlsError::ConnectionClosed => Ok(0),
                        TlsError::InsufficientSpace => Err(msg(RECORD_TOO_BIG)),
                        _ => Err(msg(lost)),
                    }
                }
            },
        }
    }

    async fn write_all(&mut self, data: &[u8]) -> Result<(), ()> {
        use embedded_io_async::Write;
        match self {
            Conn::Plain(s) => s.write_all(data).await.map_err(drop),
            Conn::Tls(t) => {
                t.write_all(data).await.map_err(drop)?;
                t.flush().await.map_err(drop)
            }
        }
    }
}

/// Diagnostics: a number for the last TLS read error (see /stats).
#[cfg(feature = "diag")]
pub static TLS_LAST_ERROR: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
#[cfg(feature = "diag")]
fn tls_error_code(e: &TlsError) -> u32 {
    match e {
        TlsError::ConnectionClosed => 1,
        TlsError::InsufficientSpace => 2,
        TlsError::CryptoError => 3,
        TlsError::InvalidRecord => 4,
        TlsError::Io(_) => 5,
        TlsError::IoError => 6,
        TlsError::ParseError(_) => 7,
        TlsError::InvalidApplicationData => 8,
        TlsError::UnknownContentType => 9,
        TlsError::HandshakeAborted(..) => 10,
        TlsError::InvalidHandshake => 11,
        TlsError::Unimplemented => 12,
        TlsError::DecodeError => 13,
        _ => 99,
    }
}

const RECORD_TOO_BIG: &str = "The server's TLS records are too big for this radio's memory; try http://";

/// Explain a failed TLS handshake.
fn tls_error(e: TlsError) -> Msg {
    match e {
        TlsError::HandshakeAborted(..) => msg("The server refused the secure connection (it may not offer TLS 1.3)"),
        TlsError::InsufficientSpace => msg(RECORD_TOO_BIG),
        TlsError::Io(_) | TlsError::IoError | TlsError::ConnectionClosed => msg("Connection lost while setting up TLS"),
        TlsError::InvalidCipherSuite | TlsError::InvalidKeyShare | TlsError::InvalidSignatureScheme => {
            msg("The server's TLS settings aren't supported (needs AES-128-GCM and P-256)")
        }
        _ => msg("The secure (TLS) connection failed"),
    }
}

/// Connect, follow redirects and playlists, then stream until stopped.
async fn fetch(stack: Stack<'static>, url: Text<256>, net: &mut [u8], tx: &mut [u8], rng: &mut Rng<'static, RNG>) -> Result<(), Fail> {
    let mut url = url;
    for _hop in 0..6 {
        let u = parse_url(url.as_str())?;
        stage(1);
        let addrs = stack
            .dns_query(u.host, DnsQueryType::A)
            .await
            .map_err(|_| msg("Couldn't look up the server's address"))?;
        let addr = *addrs.first().ok_or_else(|| msg("The server's name has no address"))?;

        let (tcp_rx, tls_buf) = if u.tls { net.split_at_mut(NET_LEN - TLS_READ - TLS_WRITE) } else { (&mut *net, &mut [][..]) };
        let mut socket = TcpSocket::new(stack, tcp_rx, &mut *tx);
        socket.set_timeout(Some(Duration::from_secs(15)));
        stage(2);
        socket.connect((addr, u.port)).await.map_err(|_| msg("Couldn't connect to the server"))?;

        let mut conn = if u.tls {
            let (read_buf, write_buf) = tls_buf.split_at_mut(TLS_READ);
            let mut tls = TlsConnection::new(socket, read_buf, write_buf);
            // RSA signatures are offered so servers with RSA certificates
            // (most of them) agree to talk; nothing is checked either way.
            // The certificate isn't verified: there's no room for root
            // certificates, and a radio stream isn't secret. This still
            // gets through to https-only servers and is private on the wire.
            let config = TlsConfig::new()
                .with_server_name(u.host)
                .with_max_fragment_length(MaxFragmentLength::Bits11)
                .enable_rsa_signatures();
            #[cfg(feature = "diag")]
            let started = Instant::now();
            stage(3);
            tls.open(TlsContext::new(&config, UnsecureProvider::new::<Aes128GcmSha256>(&mut *rng)))
                .await
                .map_err(tls_error)?;
            #[cfg(feature = "diag")]
            TLS_HANDSHAKE_MS.store(started.elapsed_sat().as_millis() as u32, portable_atomic::Ordering::Relaxed);
            Conn::Tls(tls)
        } else {
            Conn::Plain(socket)
        };

        {
            let mut req: Text<512> = Text::new();
            let _ = write!(
                req,
                "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: CH32V307-FreeNetRadio/0.1\r\nAccept: */*\r\nIcy-MetaData: 1\r\nConnection: close\r\n\r\n",
                u.path, u.host
            );
            conn.write_all(req.as_bytes()).await.map_err(|_| msg("Connection lost while sending the request"))?;
            stage(4);
        }

        // Response headers. The same buffer then holds a playlist or
        // carries the audio.
        let mut buf = [0u8; 2048];
        let read_head = async {
            let mut len = 0;
            loop {
                if len == buf.len() {
                    return Err(Fail::from(msg("The server's response headers are too long")));
                }
                let n = conn.read(&mut buf[len..], "").await.map_err(|_| retry("Connection lost while reading the response"))?;
                if n == 0 {
                    return Err(retry("The server closed the connection without replying"));
                }
                len += n;
                if let Some(i) = buf[..len].windows(4).position(|w| w == b"\r\n\r\n") {
                    return Ok((i + 4, len));
                }
            }
        };
        // Dropping a half-done TLS read on timeout is fine: the connection
        // is abandoned anyway.
        let (head_len, len) = with_timeout(RESPONSE_TIMEOUT, read_head)
            .await
            .map_err(|_| retry("The server didn't answer"))??;
        stage(5);
        let headers = &buf[..head_len];
        let status = status_code(headers).ok_or_else(|| msg("The server didn't send a valid HTTP response"))?;
        let content_type = header(headers, b"content-type").unwrap_or(b"");
        let has = |needle: &[u8]| content_type.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle));

        if (300..400).contains(&status) {
            let location = header(headers, b"location").ok_or_else(|| msg("Redirect without a Location header"))?;
            url = absolute_url(&u, location);
            continue;
        }
        if status != 200 {
            let mut m = Msg::new();
            let _ = write!(m, "The server replied with HTTP status {status}");
            return Err(m.into());
        }

        // A playlist (.m3u / .pls): play the first stream URL in it.
        if has(b"mpegurl") || has(b"scpls") || has(b"x-pls") {
            buf.copy_within(head_len..len, 0);
            let mut blen = len - head_len;
            while blen < buf.len() {
                match conn.read(&mut buf[blen..], "").await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => blen += n,
                }
            }
            url = first_url_in_playlist(&buf[..blen]).ok_or_else(|| msg("The playlist has no stream URL in it"))?;
            continue;
        }
        if has(b"aac") || has(b"mp4") {
            return Err(msg("AAC streams aren't supported, only MP3").into());
        }
        if has(b"ogg") || has(b"opus") || has(b"flac") {
            return Err(msg("Ogg/Opus/FLAC streams aren't supported, only MP3").into());
        }
        if has(b"text/html") {
            return Err(msg("That URL is a web page, not an audio stream").into());
        }

        let metaint = header(headers, b"icy-metaint")
            .and_then(|v| core::str::from_utf8(v).ok())
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        with_status(|s| {
            if let Some(name) = header(headers, b"icy-name") {
                s.station.clear();
                s.station.push_sanitized(name.trim_ascii());
            }
            // After a reconnect the audio may still be playing from the
            // buffer: leave it be.
            if s.phase != Phase::Playing {
                s.phase = Phase::Buffering;
            }
            s.message.clear();
        });

        // Audio that arrived along with the headers.
        let mut icy = Icy::new(metaint);
        icy.feed(&buf[head_len..len]).await;
        stage(6);

        loop {
            // Dropping a half-done TLS read on a stall is fine: the
            // connection is abandoned anyway.
            let read = with_timeout(STALL_TIMEOUT, conn.read(&mut buf, "")).await.map_err(|_| retry("The stream stalled"))?;
            match read.map_err(|_| retry("Lost the connection to the stream"))? {
                0 => return Ok(()),
                n => {
                    #[cfg(feature = "diag")]
                    STREAM_BYTES.fetch_add(n as u32, portable_atomic::Ordering::Relaxed);
                    icy.feed(&buf[..n]).await
                }
            }
        }
    }
    Err(msg("Too many redirects").into())
}

/// Splits an ICY stream into audio (to the pipe) and metadata (titles).
struct Icy {
    metaint: usize,
    until_meta: usize,
    state: IcyState,
    meta: [u8; 512],
    meta_len: usize,
}

enum IcyState {
    Audio,
    MetaLength,
    Meta { left: usize },
}

impl Icy {
    fn new(metaint: usize) -> Self {
        Self { metaint, until_meta: metaint, state: IcyState::Audio, meta: [0; 512], meta_len: 0 }
    }

    async fn feed(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            match self.state {
                IcyState::Audio if self.metaint == 0 => {
                    STREAM.write_all(data).await;
                    return;
                }
                IcyState::Audio => {
                    let n = self.until_meta.min(data.len());
                    STREAM.write_all(&data[..n]).await;
                    data = &data[n..];
                    self.until_meta -= n;
                    if self.until_meta == 0 {
                        self.state = IcyState::MetaLength;
                    }
                }
                IcyState::MetaLength => {
                    let len = data[0] as usize * 16;
                    data = &data[1..];
                    self.meta_len = 0;
                    if len == 0 {
                        self.state = IcyState::Audio;
                        self.until_meta = self.metaint;
                    } else {
                        self.state = IcyState::Meta { left: len };
                    }
                }
                IcyState::Meta { left } => {
                    let n = left.min(data.len());
                    let keep = n.min(self.meta.len() - self.meta_len);
                    self.meta[self.meta_len..self.meta_len + keep].copy_from_slice(&data[..keep]);
                    self.meta_len += keep;
                    data = &data[n..];
                    if left - n == 0 {
                        if let Some(title) = stream_title(&self.meta[..self.meta_len]) {
                            with_status(|s| {
                                s.title.clear();
                                s.title.push_sanitized(title);
                            });
                        }
                        self.state = IcyState::Audio;
                        self.until_meta = self.metaint;
                    } else {
                        self.state = IcyState::Meta { left: left - n };
                    }
                }
            }
        }
    }
}

/// `StreamTitle='...';` from an ICY metadata block.
fn stream_title(meta: &[u8]) -> Option<&[u8]> {
    const KEY: &[u8] = b"StreamTitle='";
    let start = meta.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    let rest = &meta[start..];
    let end = rest.windows(2).position(|w| w == b"';").unwrap_or_else(|| {
        rest.iter().rposition(|&b| b == b'\'').unwrap_or(rest.len())
    });
    Some(rest[..end].trim_ascii())
}

/// HTTP status code from the status line ("HTTP/1.x 200 OK" or "ICY 200 OK").
fn status_code(headers: &[u8]) -> Option<u16> {
    let line = headers.split(|&b| b == b'\r').next()?;
    let code = line.split(|&b| b == b' ').nth(1)?;
    core::str::from_utf8(code).ok()?.parse().ok()
}

/// Value of the first header called `name` (case-insensitive), trimmed.
fn header<'a>(head: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    head.split(|&b| b == b'\n').skip(1).find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let colon = line.iter().position(|&b| b == b':')?;
        line[..colon].eq_ignore_ascii_case(name).then(|| line[colon + 1..].trim_ascii())
    })
}

/// Resolve a Location header against the current URL.
fn absolute_url(current: &Url<'_>, location: &[u8]) -> Text<256> {
    let mut t = Text::new();
    let loc = core::str::from_utf8(location).unwrap_or("");
    if loc.starts_with("http://") || loc.starts_with("https://") {
        t.push_str(loc);
    } else {
        let scheme = if current.tls { "https" } else { "http" };
        let _ = write!(t, "{scheme}://{}:{}", current.host, current.port);
        if !loc.starts_with('/') {
            t.push_str("/");
        }
        t.push_str(loc);
    }
    t
}

/// First stream URL in an .m3u or .pls playlist.
fn first_url_in_playlist(body: &[u8]) -> Option<Text<256>> {
    for line in body.split(|&b| b == b'\n') {
        let line = core::str::from_utf8(line).ok()?.trim();
        // .pls lines look like "File1=http://..."
        let candidate = line.split_once('=').filter(|(k, _)| k.starts_with("File")).map_or(line, |(_, v)| v);
        if candidate.starts_with("http://") || candidate.starts_with("https://") {
            return Some(Text::from_str(candidate));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Decoding and output
// ---------------------------------------------------------------------------

/// Peak sample (0..=32768) to a meter level 0..=255 on a decibel scale
/// spanning -48 dB (0) to full scale (255). log2 from the bit position plus
/// 4 bits of mantissa; no floating point needed.
fn meter_level(peak: i32) -> u8 {
    if peak <= 0 {
        return 0;
    }
    let bits = 31 - peak.leading_zeros() as i32; // floor(log2)
    let mantissa = if bits >= 4 { (peak >> (bits - 4)) & 15 } else { (peak << (4 - bits)) & 15 };
    let log2_x16 = bits * 16 + mantissa; // full scale (2^15) = 240
    const RANGE_X16: i32 = 8 * 16; // 8 bits = 48 dB
    ((log2_x16 - (240 - RANGE_X16)) * 255 / RANGE_X16).clamp(0, 255) as u8
}

/// Fade length for starting and stopping: 1024 frames, ~23 ms at 44.1 kHz.
/// Without these the output would jump between silence and mid-waveform
/// audio, which clicks. (Modules without an XSMT pin can't use the DAC's
/// own soft mute for this.)
const FADE_FRAMES: usize = 1024;

/// Decodes MP3 from `STREAM` into the I2S ring, handling buffering, volume,
/// sample-rate changes and fading in/out around starts and stops.
#[embassy_executor::task]
pub async fn audio_task(
    mut i2s: I2sOut,
    decoder: &'static mut Decoder,
    window: &'static mut [u8; WINDOW],
    pcm: &'static mut [i16; MAX_SAMPLES_PER_FRAME],
) -> ! {
    let mut window_len = 0;
    let mut eq = crate::eq::Eq::new();
    let mut playing = false;
    let mut generation = u32::MAX;
    let mut decode_total_us: u64 = 0;
    let mut decode_count: u32 = 0;
    // Frames of fade-in still to apply after (re)starting playback.
    let mut fade_in_left = 0usize;
    #[cfg(feature = "diag")]
    let mut play_started = Instant::now();
    // When the pipe became full while prebuffering.
    let mut full_since: Option<Instant> = None;

    #[cfg(feature = "diag")]
    let mut last_pass = Instant::now();
    loop {
        #[cfg(feature = "diag")]
        AUDIO_LOOPS.fetch_add(1, portable_atomic::Ordering::Relaxed);
        i2s.consume_played();
        let now = Instant::now();
        #[cfg(feature = "diag")]
        MAX_LOOP_GAP_US.fetch_max(now.saturating_duration_since(last_pass).as_micros() as u32, portable_atomic::Ordering::Relaxed);
        #[cfg(feature = "diag")]
        {
            last_pass = now;
        }
        if playing {
            #[cfg(feature = "diag")]
            MIN_QUEUED.fetch_min(i2s.queued_frames() as u32, portable_atomic::Ordering::Relaxed);
        }
        let (phase, gen, volume) = with_status(|s| (s.phase, s.generation, if s.muted { 0 } else { s.volume }));

        // New stream or stopped: drop everything and go quiet.
        if gen != generation {
            generation = gen;
            window_len = 0;
            playing = false;
            i2s.fade_out(FADE_FRAMES);
        }
        if !matches!(phase, Phase::Buffering | Phase::Playing) {
            if playing {
                playing = false;
                i2s.fade_out(FADE_FRAMES);
            }
            Timer::after_millis(20).await;
            continue;
        }

        let buffered = STREAM.len() + window_len;
        let buffer_pct = (buffered * 100 / (PIPE_LEN + WINDOW)) as u8;
        with_status(|s| s.buffer_pct = buffer_pct);

        if !playing {
            if buffered < PREBUFFER {
                full_since = None;
                Timer::after_millis(20).await;
                continue;
            }
            if full_since.get_or_insert(now).elapsed_sat() < PREBUFFER_HOLD {
                Timer::after_millis(20).await;
                continue;
            }
            full_since = None;
            playing = true;
            #[cfg(feature = "diag")]
            {
                play_started = Instant::now();
            }
            i2s.flush_silence();
            fade_in_left = FADE_FRAMES;
            set_phase(Phase::Playing, "");
        }

        // Decode while there's room for a whole frame in the ring.
        let gain = (volume as i32 * volume as i32 * 32768) / 10_000; // perceptual-ish curve
        while i2s.free_frames() > MAX_SAMPLES_PER_FRAME / 2 + 64 {
            if window_len < WINDOW {
                if let Ok(n) = STREAM.try_read(&mut window[window_len..]) {
                    window_len += n;
                }
            }
            if window_len < MIN_DECODE_INPUT {
                break;
            }
            let started = Instant::now();
            let (used, result) = decoder.decode(&window[..window_len], &mut pcm[..]);
            let us = started.elapsed_sat().as_micros() as u32;
            window.copy_within(used..window_len, 0);
            window_len -= used;

            #[cfg(feature = "diag")]
            if let Err(e) = &result {
                let k = match e {
                    nanomp3_core::DecodeError::NoFrame => 0,
                    nanomp3_core::DecodeError::ReservoirUnavailable(_) => 1,
                    nanomp3_core::DecodeError::Corrupt(_) => 2,
                    _ => 3,
                };
                DECODE_ERRORS[k].fetch_add(1, portable_atomic::Ordering::Relaxed);
            }
            if let Ok(info) = result {
                decode_total_us += us as u64;
                decode_count += 1;
                if info.sample_rate != i2s.sample_rate() {
                    i2s.set_sample_rate(info.sample_rate);
                }
                let stereo = info.channels == Channels::Stereo;
                eq.update(info.sample_rate);
                let (mut peak_l, mut peak_r) = (0i32, 0i32);
                for f in 0..info.samples_produced {
                    let (l, r) = if stereo { (pcm[2 * f], pcm[2 * f + 1]) } else { (pcm[f], pcm[f]) };
                    let g = if fade_in_left > 0 {
                        fade_in_left -= 1;
                        (gain as i64 * (FADE_FRAMES - fade_in_left) as i64 / FADE_FRAMES as i64) as i32
                    } else {
                        gain
                    };
                    // Meter the music itself (before volume), so it moves
                    // even at low volume.
                    peak_l = peak_l.max((l as i32).abs());
                    peak_r = peak_r.max((r as i32).abs());
                    let (l, r) = eq.process(l, r);
                    let l = (l as i32 * g) >> 15;
                    let r = (r as i32 * g) >> 15;
                    i2s.push(l as i16, r as i16);
                }
                let (bands, n) = decoder.spectrum();
                crate::spectrum::publish(bands, n);
                decoder.reset_spectrum();
                let avg = (decode_total_us / decode_count.max(1) as u64) as u32;
                with_status(|s| {
                    s.sample_rate = I2sOut::actual_rate(info.sample_rate);
                    s.bitrate = info.bitrate;
                    s.channels = info.channels.num();
                    // Nothing on the meters when muted / volume 0.
                    s.level_l = if gain == 0 { 0 } else { meter_level(peak_l) };
                    s.level_r = if gain == 0 { 0 } else { meter_level(peak_r) };
                    s.decode_max_us = s.decode_max_us.max(us);
                    s.decode_avg_us = avg;
                });
                if decode_count >= 1000 {
                    decode_total_us /= 2;
                    decode_count /= 2;
                }
            }
            if used == 0 {
                break; // shouldn't happen; avoid spinning
            }
            // A frame takes ~12 ms to decode: let the display and network
            // run between frames rather than decoding several back to back.
            yield_now().await;
        }

        // Running dry: go back to buffering. If audio is still queued, fade
        // it out; if the DMA already caught up, the output is silent anyway.
        let starved = STREAM.len() + window_len < MIN_DECODE_INPUT;
        let queued = i2s.queued_frames();
        #[cfg(feature = "diag")]
        let buffered = STREAM.len() + window_len;
        let overtaken = i2s.resync_if_underrun(256);
        if overtaken || (starved && queued < 512) {
            #[cfg(feature = "diag")]
            critical_section::with(|cs| {
                *UNDERRUN_INFO.borrow_ref_mut(cs) =
                    [play_started.elapsed_sat().as_millis() as u32, if overtaken { 1 } else { 2 }, buffered as u32, queued as u32];
            });
            with_status(|s| s.underruns += 1);
            playing = false;
            i2s.fade_out(FADE_FRAMES);
            set_phase(Phase::Buffering, "");
        }

        Timer::after_millis(10).await;
    }
}
