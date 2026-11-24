//! Internet radio for the CH32V307V-EVT-R1: plays HTTP MP3 streams through
//! a PCM5102A I2S DAC, shows what's playing on the SSD1322 OLED, and is
//! controlled from a web page served over the built-in 10BASE-T Ethernet.
//!
//!   GET  /               web page: stream URL, play/stop, volume, status
//!   GET  /radio          player status as JSON
//!   POST /radio/play     start a stream: raw text/plain URL body, or a form
//!                        field `url=` (x-www-form-urlencoded)
//!   POST /radio/stop     stop playback
//!   POST /radio/volume   set the volume: body `0`..`100`, or form field `volume=`
//!   GET  /presets        saved station presets as JSON
//!   POST /presets/<1..99> save a preset: form fields name= and url= (empty
//!                        url clears it)
//!   POST /presets/<n>/play  play a preset
//!   POST /presets/reset  back to the built-in presets
//!   POST /remote/<key>   press a virtual remote button (1..9, 0, star, hash,
//!                        up, down, left, right, ok); append `?repeat` for a
//!                        held button's repeats
//!   GET  /licenses       the open source library list (name|author|license per line)
//!   GET  /screen         raw framebuffer dump (256x64, 4 bpp) for debugging
//!
//! Diag builds (`--features diag`) add GET /stats (uptime, stack, display
//! timing, decoder load), POST /stats/reset and GET /screen/stream (a
//! tear-free screen recording).
//!
//! POSTs reply with the status JSON, or redirect to / for plain HTML form
//! submits (requests whose Accept header includes text/html).
//!
//! Streams: http:// or https:// MP3 (Icecast/Shoutcast), including .m3u/.pls
//! playlists and redirects; "now playing" titles come from ICY metadata.
//! AAC and Ogg aren't supported and are reported as such.
//!
//! IR remote (VS1838B receiver kit): S -> PB10, + -> 3V3, - -> GND.
//! Buttons: see remote.rs.
//!
//! Wiring:
//!
//!   PCM5102A               OLED (SSD1322, 4-wire SPI)   RJ45 LEDs
//!   BCK   PB13             SCLK (D0)  PA5               ELED1 (link) PC0
//!   LCK   PB12             SDIN (D1)  PA7               ELED2 (act)  PC1
//!   DIN   PB15             CS         PA4
//!   SCK   GND              D/C        PA3
//!   FMT   GND (I2S format)   RES        PA2
//!   XSMT  high (unmuted). Modules without an XSMT pin tie it high on board;
//!         the firmware fades in/out in software to avoid clicks.
//!   VIN   3V3 or 5V, GND   GND
//!
//! Needs the chip's flash/RAM split set to 192K + 128K: run `tools/ram-split`
//! (TARGET_MODE = 0b00) once first. See memory.x.
//!
//! The PHY is 10 Mbit only: plug it into a 10/100/1000 port. Multi-gig and
//! 10G ports usually don't support 10BASE-T and the link won't come up.
//!
//! Avoid attaching the debugger (`wlink dump`/`regs`/`status`) while it runs:
//! the WCH-LinkE reconfigures the clocks on attach, which stops the PHY's
//! 60 MHz clock and drops the link until the next reset.

#![no_std]
#![no_main]

use core::fmt::Write as _;
use core::task::Context;

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use embassy_time::{Duration, Instant, Timer};

/// `Instant::elapsed` that can't panic. The HAL's SysTick time driver can
/// read one tick behind an earlier reading, and embassy-time's plain
/// subtraction panics on that (a bare trap, with panic messages stripped).
pub trait ElapsedSat {
    fn elapsed_sat(&self) -> Duration;
}
impl ElapsedSat for Instant {
    fn elapsed_sat(&self) -> Duration {
        Instant::now().saturating_duration_since(*self)
    }
}
use embedded_io_async::Write as _;
use hal::eth::{self, Ethernet, PacketQueue, Phy, PhyInterface, Smi};
use hal::gpio::{Level, Output};
use hal::spi::{self, Spi};
use hal::time::Hertz;
use hal::try_println;
use hal::{bind_interrupts, peripherals, rng};
use nanomp3_core::{Decoder, MAX_SAMPLES_PER_FRAME};
use ssd1322::{Ssd1322, BUF_LEN};
use static_cell::ConstStaticCell;
use text::Text;
use ch32_hal as hal;

mod about;
mod clock;
mod display;
mod eq;
mod i2s;
mod ir;
mod menu;
mod presets;
mod radio;
mod remote;
mod settings;
mod spectrum;
mod ssd1322;
mod text;

/// Number of web-page connections served concurrently. Extra simultaneous
/// connections are refused (RST) rather than queued.
const HTTP_TASKS: usize = 2;

/// Sockets: DHCP + DNS + the stream connection + NTP + the web server.
const SOCKETS: usize = 4 + HTTP_TASKS;

/// 2 TX + 7 RX DMA buffers (during each ~12 ms MP3 decode, plus a TLS
/// record decrypt and the TLS handshake's key exchange, the network task
/// can't run, and bursts of stream + LAN traffic overflowed 4, and 6 at
/// 320 kbit/s over https).
/// Keep the TX count even: ch32-hal packs the
/// 1514-byte TX buffers in front of the RX buffers without padding, so an odd
/// count leaves the RX buffers 2 bytes off word alignment and the Ethernet DMA
/// then writes past them, corrupting whatever follows in RAM.
type Device = Ethernet<'static, 2, 7, InternalPhy>;

bind_interrupts!(struct Irqs {
    RNG => rng::InterruptHandler<peripherals::RNG>;
});

/// Driver for the CH32V307's built-in 10BASE-T PHY.
///
/// ch32-hal's `GenericPhy` only enables the PHY's "auto" receive polarity,
/// which ch32-hal's CH32V208 driver notes doesn't link reliably. Like WCH's own
/// driver, alternate between positive and negative polarity while the link is
/// down and keep whichever one links.
struct InternalPhy {
    switched_at: Instant,
}

impl InternalPhy {
    const ADDR: u8 = 1;
    const BCR: u8 = 0x00;
    const BSR: u8 = 0x01;
    const MDIX: u8 = 0x1E;
    const BCR_RESET: u16 = 1 << 15;
    const BCR_AUTONEG: u16 = 1 << 12;
    const BCR_AUTONEG_RESTART: u16 = 1 << 9;
    const BSR_LINK_UP: u16 = 1 << 2;
    /// MDIX bits 3:2 select receive polarity: 00 positive, 01 negative.
    const MDIX_PN_MASK: u16 = 3 << 2;
    const MDIX_PN_NEGATIVE: u16 = 1 << 2;
    /// Auto-negotiation takes about 1 s; give each polarity 2 s.
    const SWITCH_AFTER: Duration = Duration::from_secs(2);

    fn new() -> Self {
        Self { switched_at: Instant::now() }
    }
}

impl Phy for InternalPhy {
    fn phy_reset(&mut self, smi: &Smi) {
        smi.write(Self::ADDR, Self::BCR, Self::BCR_RESET);
        while smi.read(Self::ADDR, Self::BCR) & Self::BCR_RESET != 0 {}
    }

    fn phy_init(&mut self, smi: &Smi) {
        let mdix = smi.read(Self::ADDR, Self::MDIX) & !Self::MDIX_PN_MASK;
        smi.write(Self::ADDR, Self::MDIX, mdix); // positive polarity
        smi.write(Self::ADDR, Self::BCR, Self::BCR_AUTONEG | Self::BCR_AUTONEG_RESTART);
        self.switched_at = Instant::now();
    }

    fn poll_link(&mut self, smi: &Smi, cx: &mut Context) -> bool {
        let up = smi.read(Self::ADDR, Self::BSR) & Self::BSR_LINK_UP != 0;
        jack_leds::set_link(up);
        display::set_link(up);
        if up {
            return true;
        }
        if self.switched_at.elapsed_sat() >= Self::SWITCH_AFTER {
            let mdix = smi.read(Self::ADDR, Self::MDIX);
            let pn = if mdix & Self::MDIX_PN_MASK == Self::MDIX_PN_NEGATIVE { 0 } else { Self::MDIX_PN_NEGATIVE };
            smi.write(Self::ADDR, Self::MDIX, (mdix & !Self::MDIX_PN_MASK) | pn);
            let bcr = smi.read(Self::ADDR, Self::BCR);
            smi.write(Self::ADDR, Self::BCR, bcr | Self::BCR_AUTONEG_RESTART);
            self.switched_at = Instant::now();
        }
        // Nothing else wakes the network stack while the link is down,
        // so ask to be polled again.
        cx.waker().wake_by_ref();
        false
    }
}

/// Times the Ethernet RX ring was full when a frame arrived (frames dropped).
#[cfg(feature = "diag")]
static RX_OVERFLOWS: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);

#[hal::interrupt]
unsafe fn ETH() {
    // WCH extension: DMASR bit 31 flags an internal-PHY link status change.
    // ch32-hal never clears it, so after the first link drop/restore the
    // interrupt re-fires forever. It isn't raised for every link-down, so
    // link loss is detected by `jack_leds::tick` polling instead.
    const PHY_LINK_CHANGED: u32 = 1 << 31;
    let dmasr = hal::pac::ETH.dmasr().read().0;
    let link_changed = dmasr & PHY_LINK_CHANGED != 0;
    // RX buffer unavailable: a frame arrived with every RX buffer full.
    #[cfg(feature = "diag")]
    if dmasr & (1 << 7) != 0 {
        RX_OVERFLOWS.fetch_add(1, portable_atomic::Ordering::Relaxed);
    }
    jack_leds::on_eth_interrupt(link_changed);
    eth::on_interrupt();
    // ch32-hal enables the abnormal-summary and RX-buffer-unavailable
    // interrupts but its handler only clears TS/RS/NIS. Once a burst fills the
    // RX ring, RBUS/AIS stay set and the interrupt re-fires forever, starving
    // the executor. Clear the abnormal (write-1-to-clear) flags too; the RX
    // ring restarts on the driver's next demand poll.
    hal::pac::ETH.dmasr().write(|w| {
        w.set_tpss(true);
        w.set_tbus(true);
        w.set_tjts(true);
        w.set_ros(true);
        w.set_tus(true);
        w.set_rbus(true);
        w.set_rpss(true);
        w.set_rwts(true);
        w.set_ets(true);
        w.set_fbes(true);
        w.set_ers(true);
        w.set_ais(true);
        w.0 |= PHY_LINK_CHANGED;
    });
}

/// Runs from boot (not just once DHCP is up) so the activity LED is turned
/// off between frames and the link is re-checked even while the link is down.
#[embassy_executor::task]
async fn jack_leds_task() -> ! {
    loop {
        Timer::after_millis(25).await;
        jack_leds::tick();
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Device>) -> ! {
    runner.run().await
}

#[embassy_executor::main(entry = "ch32_hal::entry")]
async fn main(spawner: Spawner) -> ! {
    #[cfg(feature = "diag")]
    stack_usage::paint();
    hal::debug::SDIPrint::enable();
    let p = hal::init(hal::Config {
        rcc: hal::rcc::Config::SYSCLK_FREQ_144MHZ_HSE,
        ..Default::default()
    });

    jack_leds::init(p.PC0, p.PC1);
    spawner.spawn(jack_leds_task().unwrap());

    // SSD1322 on SPI1. Max SCLK is 10 MHz; 144 MHz / 16 = 9 MHz. Frames go
    // out by DMA (DMA1 channel 3 = SPI1 TX).
    let mut spi_config = spi::Config::default();
    spi_config.frequency = Hertz::mhz(9);
    let spi = Spi::new_txonly(p.SPI1, p.PA5, p.PA7, p.DMA1_CH3, spi_config);
    let cs = Output::new(p.PA4, Level::High, Default::default());
    let dc = Output::new(p.PA3, Level::Low, Default::default());
    let rst = Output::new(p.PA2, Level::High, Default::default());
    // ConstStaticCell keeps big buffers off the stack (StaticCell::init(value)
    // would build them on the stack first and overflow it).
    static FRAMEBUFFER: ConstStaticCell<[u8; BUF_LEN]> = ConstStaticCell::new([0; BUF_LEN]);
    let framebuffer = FRAMEBUFFER.take();
    SCREEN.store(framebuffer.as_mut_ptr(), portable_atomic::Ordering::Relaxed);
    let oled = Ssd1322::new(spi, cs, dc, rst, framebuffer);
    spawner.spawn(display::display_task(oled).unwrap());

    // PCM5102A on I2S2 (SPI2), DMA1 channel 5.
    let i2s = i2s::I2sOut::new(p.SPI2, p.PB12, p.PB13, p.PB15, 44_100);
    static DECODER: ConstStaticCell<Decoder> = ConstStaticCell::new(Decoder::new());
    static WINDOW: ConstStaticCell<[u8; radio::WINDOW]> = ConstStaticCell::new([0; radio::WINDOW]);
    static PCM: ConstStaticCell<[i16; MAX_SAMPLES_PER_FRAME]> = ConstStaticCell::new([0; MAX_SAMPLES_PER_FRAME]);
    spawner.spawn(radio::audio_task(i2s, DECODER.take(), WINDOW.take(), PCM.take()).unwrap());

    // Station presets, saved in the last 4 KB of flash.
    presets::init(hal::flash::Flash::new_blocking(p.FLASH));

    // IR remote receiver on PB10.
    ir::init(p.PB10);
    spawner.spawn(ir::ir_task().unwrap());
    spawner.spawn(remote::digit_task().unwrap());

    // Stable, locally administered MAC derived from the chip's unique ID.
    let uid = hal::signature::unique_id();
    let mac = [0x02, uid[0] ^ uid[6], uid[1] ^ uid[7], uid[2] ^ uid[8], uid[3] ^ uid[9], uid[4] ^ uid[10]];
    let seed = u64::from_le_bytes(uid[..8].try_into().unwrap());

    static QUEUE: ConstStaticCell<PacketQueue<2, 7>> = ConstStaticCell::new(PacketQueue::new());
    // Last arg is HCLK in MHz (sets the SMI clock divider).
    let device = Ethernet::new(QUEUE.take(), mac, InternalPhy::new(), PhyInterface::Internal10M, 144);

    static RESOURCES: ConstStaticCell<StackResources<SOCKETS>> = ConstStaticCell::new(StackResources::new());
    let (stack, runner) = embassy_net::new(device, embassy_net::Config::dhcpv4(Default::default()), RESOURCES.take(), seed);
    spawner.spawn(net_task(runner).unwrap());

    // Hardware random numbers for TLS key exchange.
    let rng = rng::Rng::new(p.RNG, Irqs);
    // Last station, volume and mute from before power-off; resumes playing
    // if it was playing. Started before the network is up: the stream task
    // shows that it's waiting for the network, then plays.
    settings::restore();
    spawner.spawn(radio::stream_task(stack, rng).unwrap());
    spawner.spawn(settings::save_task().unwrap());
    spawner.spawn(clock::ntp_task(stack).unwrap());

    try_println!("waiting for link + DHCP...");
    stack.wait_config_up().await;
    if let Some(cfg) = stack.config_v4() {
        try_println!("Serving on http://{}/", cfg.address.address());
    }
    display::set_ip(stack.config_v4().map(|c| c.address.address().octets()));
    for _ in 0..HTTP_TASKS {
        spawner.spawn(http_task(stack).unwrap());
    }

    // Keep the IP shown on the display current (DHCP can change or lose it).
    loop {
        display::set_ip(stack.config_v4().map(|c| c.address.address().octets()));
        Timer::after_secs(1).await;
    }
}

#[embassy_executor::task(pool_size = HTTP_TASKS)]
async fn http_task(stack: Stack<'static>) -> ! {
    let mut rx = [0u8; 512];
    let mut tx = [0u8; 512];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if socket.accept(80).await.is_err() {
            continue;
        }
        // The whole request must arrive and be answered within 5 s, so a
        // stalled or trickling client can't hold this task (there are only
        // two) indefinitely.
        let _ = embassy_time::with_timeout(Duration::from_secs(if cfg!(feature = "diag") { 60 } else { 5 }), serve(&mut socket)).await;
        socket.close();
        let _ = socket.flush().await;
        socket.abort();
    }
}

async fn serve(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    // Read the request headers, then the body (if any) as given by
    // Content-Length. Requests larger than the buffer are rejected.
    // 1024: room for browser headers plus a full preset (URL-encoded).
    let mut req = [0u8; 1024];
    let mut len = 0;
    let head_len = loop {
        if len == req.len() {
            return respond(socket, "413 Payload Too Large", "text/plain", ["request too large\n"].into_iter(), None).await;
        }
        let n = socket.read(&mut req[len..]).await?;
        if n == 0 {
            return Ok(());
        }
        len += n;
        if let Some(i) = req[..len].windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let body_len = header(&req[..head_len], b"content-length")
        .and_then(|v| core::str::from_utf8(v).ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if head_len + body_len > req.len() {
        return respond(socket, "413 Payload Too Large", "text/plain", ["request too large\n"].into_iter(), None).await;
    }
    while len < head_len + body_len {
        let n = socket.read(&mut req[len..head_len + body_len]).await?;
        if n == 0 {
            return Ok(());
        }
        len += n;
    }
    let body = &req[head_len..head_len + body_len];

    let line = req[..len].split(|&b| b == b'\r').next().unwrap_or(&[]);
    let mut parts = line.split(|&b| b == b' ');
    let method = parts.next().unwrap_or(&[]);
    let path = parts.next().unwrap_or(&[]);
    let form = header(&req[..head_len], b"content-type")
        .is_some_and(|v| v.windows(33).any(|w| w.eq_ignore_ascii_case(b"application/x-www-form-urlencoded")));
    // Value posted either as a form field or as the raw body (also used when
    // a form-encoded body has no such field, e.g. `curl --data <url>`).
    let posted = |field: &[u8]| -> Text<256> {
        let mut t = Text::new();
        match form_field(body, field).filter(|_| form) {
            Some(v) => url_decode(v, &mut t),
            None => t.push_str(core::str::from_utf8(body).unwrap_or("")),
        }
        t
    };

    match (method, path) {
        (b"GET", b"/") => {
            // Every browser accepts gzip; there's no room for a plain copy.
            let mut head: Text<192> = Text::new();
            let _ = write!(
                head,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                PAGE_GZ.len()
            );
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(&PAGE_GZ).await
        }
        (b"GET", b"/presets") => {
            // Streamed as it's built (no Content-Length; the connection
            // closes at the end), to avoid a ~3 KB buffer per web task.
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n[")
                .await?;
            for i in 0..presets::SLOTS {
                let p = presets::get(i);
                let mut j: Text<320> = Text::new();
                if i > 0 {
                    j.push_str(",");
                }
                j.push_str("{\"name\":");
                json_str(&mut j, p.name.as_str());
                j.push_str(",\"url\":");
                json_str(&mut j, p.url.as_str());
                j.push_str("}");
                socket.write_all(j.as_bytes()).await?;
            }
            socket.write_all(b"]").await
        }
        (b"POST", b"/presets/reset") => {
            let result = presets::reset().await;
            preset_reply(socket, &req[..head_len], result).await
        }
        (b"POST", p) if p.starts_with(b"/presets/") && p.ends_with(b"/play") => {
            // POST /presets/<n>/play: play it (the web page's Play buttons).
            let n = &p[b"/presets/".len()..p.len() - b"/play".len()];
            let result = match core::str::from_utf8(n).ok().and_then(|n| n.parse::<usize>().ok()) {
                Some(n) if (1..=presets::SLOTS).contains(&n) => {
                    remote::play_preset(n - 1);
                    Ok(())
                }
                _ => Err("No such preset"),
            };
            preset_reply(socket, &req[..head_len], result).await
        }
        (b"POST", p) if p.starts_with(b"/presets/") => {
            // POST /presets/<1..99> with form fields name= and url=.
            let slot = core::str::from_utf8(&p[b"/presets/".len()..]).ok().and_then(|n| n.parse::<usize>().ok());
            let result = match slot {
                Some(n) if (1..=presets::SLOTS).contains(&n) => {
                    let mut name: Text<{ presets::NAME_MAX }> = Text::new();
                    let mut url: Text<{ presets::URL_MAX }> = Text::new();
                    url_decode(form_field(body, b"name").unwrap_or(b""), &mut name);
                    url_decode(form_field(body, b"url").unwrap_or(b""), &mut url);
                    presets::save(n - 1, name.as_str().trim(), url.as_str().trim()).await
                }
                _ => Err("No such preset"),
            };
            preset_reply(socket, &req[..head_len], result).await
        }
        (b"POST", p) if p.starts_with(b"/remote/") => {
            // The web page's virtual remote: same codes and actions as IR.
            let rest = &p[b"/remote/".len()..];
            let (name, repeat) = match rest.iter().position(|&b| b == b'?') {
                Some(i) => (&rest[..i], &rest[i + 1..] == b"repeat"),
                None => (rest, false),
            };
            match remote::command_for(name) {
                Some(command) => {
                    remote::on_press(ir::Press { address: 0, command, repeat });
                    Timer::after_millis(20).await;
                    if accepts_html(&req[..head_len]) {
                        respond(socket, "303 See Other", "text/plain", ["ok\n"].into_iter(), Some("/")).await
                    } else {
                        send_status(socket).await
                    }
                }
                None => respond(socket, "404 Not Found", "text/plain", ["no such button\n"].into_iter(), None).await,
            }
        }
        (b"GET", b"/radio") => {
            send_status(socket).await
        }
        (b"GET", b"/licenses") => {
            respond(socket, "200 OK", "text/plain; charset=utf-8", [about::text()].into_iter(), None).await
        }
        (b"POST", b"/radio/play") | (b"POST", b"/radio/stop") | (b"POST", b"/radio/volume") => {
            match path {
                b"/radio/play" => radio::play(posted(b"url").as_str()),
                b"/radio/stop" => radio::stop(),
                _ => {
                    if let Ok(v) = posted(b"volume").as_str().trim().parse::<u32>() {
                        radio::set_volume(v.min(100) as u8);
                    }
                }
            }
            // Let the stream task pick up a play/stop before reporting status.
            Timer::after_millis(20).await;
            if accepts_html(&req[..head_len]) {
                respond(socket, "303 See Other", "text/plain", ["ok\n"].into_iter(), Some("/")).await
            } else {
                send_status(socket).await
            }
        }
        #[cfg(feature = "diag")]
        (b"GET", p) if p.starts_with(b"/screen/stream") => {
            // Screen recording: `n` consecutive frames, each captured whole
            // (the display waits for every copy), 8192 bytes apiece.
            let n = p.split(|&c| c == b'=').nth(1).and_then(|v| core::str::from_utf8(v).ok()).and_then(|v| v.parse::<u32>().ok()).unwrap_or(100);
            let ptr = SCREEN.load(portable_atomic::Ordering::Relaxed);
            if ptr.is_null() {
                return respond(socket, "503 Service Unavailable", "text/plain", ["not ready\n"].into_iter(), None).await;
            }
            let fb = unsafe { core::slice::from_raw_parts(ptr as *const u8, BUF_LEN) };
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n").await?;
            display::SNAP_READY.reset();
            display::SNAP_DONE.reset();
            display::CAPTURE.store(true, portable_atomic::Ordering::Relaxed);
            let mut result = Ok(());
            for _ in 0..n {
                if embassy_time::with_timeout(Duration::from_secs(2), display::SNAP_READY.wait()).await.is_err() {
                    break;
                }
                result = socket.write_all(fb).await;
                display::SNAP_DONE.signal(());
                if result.is_err() {
                    break;
                }
            }
            display::CAPTURE.store(false, portable_atomic::Ordering::Relaxed);
            display::SNAP_DONE.signal(());
            result
        }
        (b"GET", b"/screen") => {
            // Debug aid: the display's raw framebuffer (256x64, 4 bits per
            // pixel, left pixel in the high nibble), read in place. It may
            // catch a frame mid-draw; that's fine for a screenshot.
            let ptr = SCREEN.load(portable_atomic::Ordering::Relaxed);
            if ptr.is_null() {
                return respond(socket, "503 Service Unavailable", "text/plain", ["not ready\n"].into_iter(), None).await;
            }
            let fb = unsafe { core::slice::from_raw_parts(ptr as *const u8, BUF_LEN) };
            let mut head: Text<160> = Text::new();
            let _ = write!(
                head,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {BUF_LEN}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(fb).await
        }
        #[cfg(feature = "diag")]
        (b"POST", b"/stats/reset") => {
            // Restart the high/low-water marks, to measure one activity.
            radio::MIN_QUEUED.store(u32::MAX, portable_atomic::Ordering::Relaxed);
            radio::MAX_LOOP_GAP_US.store(0, portable_atomic::Ordering::Relaxed);
            display::OVERLAY_MAX_US.store(0, portable_atomic::Ordering::Relaxed);
            respond(socket, "200 OK", "text/plain", ["ok\n"].into_iter(), None).await
        }
        #[cfg(feature = "diag")]
        (b"GET", b"/stats") => {
            // One shared buffer rather than one per web task (RAM is tight).
            let mut body = STATS_BODY.lock().await;
            body.clear();
            let (free, total) = stack_usage::unused();
            let gaps = display::take_frame_gaps_ms();
            let s = radio::status();
            let fail = critical_section::with(|cs| *radio::FAIL_INFO.borrow_ref(cs));
            let underrun = critical_section::with(|cs| *radio::UNDERRUN_INFO.borrow_ref(cs));
            let dbs = critical_section::with(|cs| spectrum::DEBUG_DB.borrow(cs).get());
            let _ = write!(
                body,
                "stack: {} of {} bytes never used\nuptime: {} s\nframe gap: {}-{} ms\ndecode per frame: avg {} us, max {} us\nunderruns: {}\nrx ring overflows: {}\nstream bytes in: {}\naudio loops: {}\ndisplay frames: {} (stage {}), flush timeouts: {}, overlay draw max: {} us\nDAC queue low-water: {} frames, audio loop max gap: {} us\nir: last code {}, frames {}, errors {}\ntls handshake: {} ms, longest read {} us, last error {}, stage {}, at error: tcp rx {} tx {}, tls decoded {} pending {}\nreconnects: {}, network outages: {}\nsettings saves: {}, failed {} ({})\nlast underrun (ms, kind, buffered, queued): {} {} {} {}\nspectrum dB (bands 0 10 20 30 39): {} {} {} {} {}\n",
                free,
                total,
                Instant::now().as_secs(),
                gaps.0,
                gaps.1,
                s.decode_avg_us,
                radio::take_decode_max_us(),
                s.underruns,
                RX_OVERFLOWS.load(portable_atomic::Ordering::Relaxed),
                radio::STREAM_BYTES.load(portable_atomic::Ordering::Relaxed),
                radio::AUDIO_LOOPS.load(portable_atomic::Ordering::Relaxed),
                display::FRAMES.load(portable_atomic::Ordering::Relaxed),
                display::STAGE.load(portable_atomic::Ordering::Relaxed),
                display::FLUSH_TIMEOUTS.load(portable_atomic::Ordering::Relaxed),
                display::OVERLAY_MAX_US.load(portable_atomic::Ordering::Relaxed),
                radio::MIN_QUEUED.swap(u32::MAX, portable_atomic::Ordering::Relaxed),
                radio::MAX_LOOP_GAP_US.swap(0, portable_atomic::Ordering::Relaxed),
                IrCode(ir::LAST_CODE.load(portable_atomic::Ordering::Relaxed)),
                ir::FRAMES.load(portable_atomic::Ordering::Relaxed),
                ir::ERRORS.load(portable_atomic::Ordering::Relaxed),
                radio::TLS_HANDSHAKE_MS.load(portable_atomic::Ordering::Relaxed),
                radio::TLS_READ_MAX_US.swap(0, portable_atomic::Ordering::Relaxed),
                radio::TLS_LAST_ERROR.load(portable_atomic::Ordering::Relaxed),
                radio::CONNECT_STAGE.load(portable_atomic::Ordering::Relaxed),
                fail[0], fail[1], fail[2], fail[3],
                radio::RECONNECTS.load(portable_atomic::Ordering::Relaxed),
                radio::OUTAGES.load(portable_atomic::Ordering::Relaxed),
                settings::SAVES.load(portable_atomic::Ordering::Relaxed),
                settings::FAILURES.load(portable_atomic::Ordering::Relaxed),
                critical_section::with(|cs| settings::LAST_ERROR.borrow(cs).get()),
                underrun[0],
                underrun[1],
                underrun[2],
                underrun[3],
                dbs[0],
                dbs[1],
                dbs[2],
                dbs[3],
                dbs[4]
            );
            #[cfg(feature = "diag")]
            let _ = write!(
                body,
                "i2s underruns: {}, dma errors: {}\ndecode errors (noframe, reservoir, corrupt, other): {} {} {} {}\n",
                i2s::I2S_UNDERRUNS.load(portable_atomic::Ordering::Relaxed),
                i2s::DMA_ERRORS.load(portable_atomic::Ordering::Relaxed),
                radio::DECODE_ERRORS[0].load(portable_atomic::Ordering::Relaxed),
                radio::DECODE_ERRORS[1].load(portable_atomic::Ordering::Relaxed),
                radio::DECODE_ERRORS[2].load(portable_atomic::Ordering::Relaxed),
                radio::DECODE_ERRORS[3].load(portable_atomic::Ordering::Relaxed),
            );
            respond(socket, "200 OK", "text/plain", [body.as_str()].into_iter(), None).await
        }
        (_, b"/") | (_, b"/radio") | (_, b"/radio/play") | (_, b"/radio/stop") | (_, b"/radio/volume") => {
            respond(socket, "405 Method Not Allowed", "text/plain", ["method not allowed\n"].into_iter(), None).await
        }
        #[cfg(feature = "diag")]
        (_, b"/stats") => {
            respond(socket, "405 Method Not Allowed", "text/plain", ["method not allowed\n"].into_iter(), None).await
        }
        _ => respond(socket, "404 Not Found", "text/plain", ["not found\n"].into_iter(), None).await,
    }
}

/// Buffer for GET /stats, shared by the web tasks.
#[cfg(feature = "diag")]
static STATS_BODY: embassy_sync::mutex::Mutex<embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, Text<960>> =
    embassy_sync::mutex::Mutex::new(Text::new());

/// The display framebuffer, for GET /screen.
static SCREEN: portable_atomic::AtomicPtr<u8> = portable_atomic::AtomicPtr::new(core::ptr::null_mut());

/// Last IR code for /stats: "none", or address/command in hex.
#[cfg(feature = "diag")]
struct IrCode(u32);

#[cfg(feature = "diag")]
impl core::fmt::Display for IrCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == u32::MAX {
            f.write_str("none")
        } else {
            write!(f, "addr 0x{:02X} cmd 0x{:02X}", self.0 >> 8, self.0 & 0xFF)
        }
    }
}

/// Reply to a preset change: redirect browsers, otherwise ok / error text.
async fn preset_reply(socket: &mut TcpSocket<'_>, head: &[u8], result: Result<(), &'static str>) -> Result<(), embassy_net::tcp::Error> {
    match result {
        Ok(()) if accepts_html(head) => respond(socket, "303 See Other", "text/plain", ["ok\n"].into_iter(), Some("/")).await,
        Ok(()) => respond(socket, "200 OK", "text/plain", ["ok\n"].into_iter(), None).await,
        Err(e) => respond(socket, "500 Internal Server Error", "text/plain", [e, "\n"].into_iter(), None).await,
    }
}

/// Player status as JSON for the web page.
/// One status buffer for both web tasks (RAM is tight). It is held while the
/// reply is written, so the other task's status request may wait for it; the
/// 5 s request deadline bounds that.
static STATUS_BODY: embassy_sync::mutex::Mutex<embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, Text<1024>> =
    embassy_sync::mutex::Mutex::new(Text::new());

async fn send_status(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    let mut j = STATUS_BODY.lock().await;
    j.clear();
    status_json(&mut j);
    respond(socket, "200 OK", "application/json", [j.as_str()].into_iter(), None).await
}

fn status_json(j: &mut Text<1024>) {
    let s = radio::status();
    let _ = write!(j, "{{\"state\":\"{}\",\"message\":", s.phase.as_str());
    json_str(j, s.message.as_str());
    j.push_str(",\"url\":");
    json_str(j, s.url.as_str());
    j.push_str(",\"station\":");
    json_str(j, s.station.as_str());
    j.push_str(",\"title\":");
    json_str(j, s.title.as_str());
    let _ = write!(
        j,
        ",\"sample_rate\":{},\"bitrate\":{},\"channels\":{},\"buffer\":{},\"underruns\":{},\"volume\":{},\"muted\":{}}}",
        s.sample_rate, s.bitrate, s.channels, s.buffer_pct, s.underruns, s.volume, s.muted
    );
}

/// Append `s` as a JSON string (it's already printable ASCII).
fn json_str<const N: usize>(out: &mut Text<N>, s: &str) {
    out.push_str("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => {
                let _ = out.write_char(c);
            }
        }
    }
    out.push_str("\"");
}

/// URL-decode a form value (`+` and `%XX`) into `out`.
fn url_decode<const N: usize>(input: &[u8], out: &mut Text<N>) {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut bytes = [0u8; 256];
    let mut n = 0;
    let mut i = 0;
    while i < input.len() && n < bytes.len() {
        let mut b = input[i];
        i += 1;
        if b == b'+' {
            b = b' ';
        } else if b == b'%' && i + 2 <= input.len() {
            if let (Some(h), Some(l)) = (hex(input[i]), hex(input[i + 1])) {
                b = h << 4 | l;
                i += 2;
            }
        }
        bytes[n] = b;
        n += 1;
    }
    out.push_str(core::str::from_utf8(&bytes[..n]).unwrap_or(""));
}

/// True if the request's `Accept` header asks for HTML, i.e. it's a browser
/// navigation (form submit) rather than `fetch()` or curl.
fn accepts_html(req: &[u8]) -> bool {
    req.split(|&b| b == b'\n').skip(1).any(|line| {
        line.len() > 7
            && line[..7].eq_ignore_ascii_case(b"accept:")
            && line[7..].windows(9).any(|w| w.eq_ignore_ascii_case(b"text/html"))
    })
}

/// Value of the first header called `name` (case-insensitive), without the
/// trailing CR.
fn header<'a>(head: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    head.split(|&b| b == b'\n').skip(1).find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let colon = line.iter().position(|&b| b == b':')?;
        line[..colon].eq_ignore_ascii_case(name).then(|| &line[colon + 1..])
    })
}

/// Raw (still URL-encoded) value of `name` in an
/// application/x-www-form-urlencoded body.
fn form_field<'a>(body: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    body.split(|&b| b == b'&').find_map(|pair| {
        let eq = pair.iter().position(|&b| b == b'=')?;
        (&pair[..eq] == name).then(|| &pair[eq + 1..])
    })
}

/// The web page, gzip-compressed at build time (see build.rs). Kept in slow
/// flash: it's only read when a browser loads the page.
#[link_section = ".slow.page"]
static PAGE_GZ: [u8; include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz")).len()] =
    *include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));

async fn respond<'b>(
    socket: &mut TcpSocket<'_>,
    status: &str,
    content_type: &str,
    body: impl Iterator<Item = &'b str> + Clone,
    location: Option<&str>,
) -> Result<(), embassy_net::tcp::Error> {
    let len: usize = body.clone().map(str::len).sum();
    let mut head = Buf::<256>::new();
    let _ = write!(
        head,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nCache-Control: no-store\r\nConnection: close\r\n"
    );
    if let Some(loc) = location {
        let _ = write!(head, "Location: {loc}\r\n");
    }
    let _ = head.write_str("\r\n");
    socket.write_all(head.as_bytes()).await?;
    for piece in body {
        socket.write_all(piece.as_bytes()).await?;
    }
    Ok(())
}

/// Fixed-size buffer implementing `core::fmt::Write` (output is truncated if full).
struct Buf<const N: usize> {
    data: [u8; N],
    len: usize,
}

impl<const N: usize> Buf<N> {
    fn new() -> Self {
        Self { data: [0; N], len: 0 }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

impl<const N: usize> core::fmt::Write for Buf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(N - self.len);
        self.data[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        if n < s.len() { Err(core::fmt::Error) } else { Ok(()) }
    }
}

/// Panic message and location, kept in RAM (symbol `PANIC_MSG`) so it can be
/// read with a debugger after the fact, and printed over the SDI console.
#[no_mangle]
static mut PANIC_MSG: Buf<192> = Buf { data: [0; 192], len: 0 };

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    critical_section::with(|_| unsafe {
        let msg = &mut *core::ptr::addr_of_mut!(PANIC_MSG);
        let _ = write!(msg, "{info}");
        try_println!("PANIC: {}", core::str::from_utf8_unchecked(msg.as_bytes()));
    });
    loop {
        core::hint::spin_loop();
    }
}

/// Stack high-water-mark measurement: fill the unused stack with a pattern at
/// boot, later count how much of it is still intact.
#[cfg(feature = "diag")]
mod stack_usage {
    const PATTERN: u32 = 0xA5A5_A5A5;

    extern "C" {
        static mut _ebss: u32;
        static _stack_start: u32;
    }

    fn bottom() -> *mut u32 {
        // Stack grows down from _stack_start towards the end of .bss.
        core::ptr::addr_of_mut!(_ebss)
    }

    fn top() -> usize {
        core::ptr::addr_of!(_stack_start) as usize
    }

    pub fn paint() {
        let sp: usize;
        unsafe { core::arch::asm!("mv {}, sp", out(reg) sp) };
        let mut p = bottom();
        while (p as usize) < sp - 256 {
            unsafe {
                p.write_volatile(PATTERN);
                p = p.add(1);
            }
        }
    }

    /// (bytes never touched, total stack bytes)
    pub fn unused() -> (usize, usize) {
        let start = bottom();
        let mut p = start;
        while (p as usize) < top() && unsafe { p.read_volatile() } == PATTERN {
            p = unsafe { p.add(1) };
        }
        (p as usize - start as usize, top() - start as usize)
    }
}

/// The RJ45 jack's LEDs (ELED1/ELED2 header pins) aren't driven by the PHY:
/// jumper ELED1 -> PC0 (link) and ELED2 -> PC1 (activity).
mod jack_leds {
    use core::cell::RefCell;
    use portable_atomic::{AtomicBool, AtomicU32, Ordering};

    use critical_section::Mutex;
    use embassy_time::Instant;
    use hal::gpio::{Level, Output};
    use hal::peripherals::{PC0, PC1};
    use hal::Peri;

    use super::hal;

    /// Like LED1, the jack LEDs light when their pin is driven low.
    const ACTIVE_LOW: bool = true;
    /// While there's traffic the activity LED blinks with this on/off time
    /// (like a PHY-driven LED), so heavy traffic doesn't look solid.
    const BLINK_MS: u32 = 50;

    static LINK: Mutex<RefCell<Option<Output<'static>>>> = Mutex::new(RefCell::new(None));
    static ACT: Mutex<RefCell<Option<Output<'static>>>> = Mutex::new(RefCell::new(None));
    /// Set by the ETH interrupt when a frame was sent or received.
    static ACTIVITY: AtomicBool = AtomicBool::new(false);
    /// When the activity LED last changed, and whether it's lit.
    static ACT_CHANGED_MS: AtomicU32 = AtomicU32::new(0);
    static ACT_LIT: AtomicBool = AtomicBool::new(false);
    static LAST_LINK_POLL_MS: AtomicU32 = AtomicU32::new(0);
    /// Set when `tick` pends the ETH interrupt itself, so it isn't counted as traffic.
    static POKED: AtomicBool = AtomicBool::new(false);
    /// Re-check the link this often (see `tick`).
    const LINK_POLL_MS: u32 = 250;

    fn level(on: bool) -> Level {
        Level::from(on != ACTIVE_LOW)
    }

    fn now_ms() -> u32 {
        Instant::now().as_millis() as u32
    }

    fn set(led: &Mutex<RefCell<Option<Output<'static>>>>, on: bool) {
        critical_section::with(|cs| {
            if let Some(led) = led.borrow_ref_mut(cs).as_mut() {
                led.set_level(level(on));
            }
        });
    }

    pub fn init(link: Peri<'static, PC0>, act: Peri<'static, PC1>) {
        let link = Output::new(link, level(false), Default::default());
        let act = Output::new(act, level(false), Default::default());
        critical_section::with(|cs| {
            LINK.borrow_ref_mut(cs).replace(link);
            ACT.borrow_ref_mut(cs).replace(act);
        });
    }

    pub fn set_link(up: bool) {
        set(&LINK, up);
    }

    /// Called from the ETH interrupt. Records traffic for the activity LED unless the
    /// interrupt was only our own link poll or a PHY link-change event.
    pub fn on_eth_interrupt(link_changed: bool) {
        let poked = POKED.swap(false, Ordering::Relaxed);
        if poked || link_changed {
            return;
        }
        ACTIVITY.store(true, Ordering::Relaxed);
    }

    /// Call every ~25 ms: runs the activity blink, and periodically pends the
    /// ETH interrupt so the network stack re-polls the link state (otherwise
    /// it only does so when traffic wakes it, and a pulled cable would leave
    /// the link LED on).
    pub fn tick() {
        let now = now_ms();
        if now.wrapping_sub(ACT_CHANGED_MS.load(Ordering::Relaxed)) >= BLINK_MS {
            // Lit -> always turn off after one blink period. Off -> light up
            // only if there was traffic since the last blink.
            let lit = ACT_LIT.load(Ordering::Relaxed);
            let light = !lit && ACTIVITY.swap(false, Ordering::Relaxed);
            if lit || light {
                set(&ACT, light);
                ACT_LIT.store(light, Ordering::Relaxed);
                ACT_CHANGED_MS.store(now, Ordering::Relaxed);
            }
        }
        if now.wrapping_sub(LAST_LINK_POLL_MS.load(Ordering::Relaxed)) >= LINK_POLL_MS {
            LAST_LINK_POLL_MS.store(now, Ordering::Relaxed);
            POKED.store(true, Ordering::Relaxed);
            unsafe { qingke::pfic::pend_interrupt(hal::pac::Interrupt::ETH as u8) };
        }
    }
}
