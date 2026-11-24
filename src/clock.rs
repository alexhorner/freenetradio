//! Wall-clock time from NTP (pool.ntp.org), shown as HH:MM in the status bar.
//!
//! Nothing is shown until the first successful sync. After that the time
//! keeps running from the local timer and is re-synced every hour (a failed
//! re-sync just keeps the old time). The time shown is UTC plus the chosen
//! offset (settings menu, saved by settings.rs), plus one hour while DST is
//! enabled and in effect (EU/UK rule: from 01:00 UTC on the last Sunday of
//! March to 01:00 UTC on the last Sunday of October).

use core::cell::Cell;

use portable_atomic::{AtomicBool, AtomicI32, Ordering};

use critical_section::Mutex;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, Stack};
use embassy_time::{with_timeout, Duration, Instant, Timer};

use crate::ElapsedSat;

const SERVER: &str = "pool.ntp.org";
/// Offset limits, in quarter hours: UTC-12:00 ..= UTC+14:00.
pub const MIN_OFFSET_QH: i32 = -48;
pub const MAX_OFFSET_QH: i32 = 56;

/// Chosen offset from UTC in quarter hours, and whether DST is enabled.
static OFFSET_QH: AtomicI32 = AtomicI32::new(0);
static DST_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn offset_qh() -> i32 {
    OFFSET_QH.load(Ordering::Relaxed)
}
pub fn set_offset_qh(qh: i32) {
    OFFSET_QH.store(qh.clamp(MIN_OFFSET_QH, MAX_OFFSET_QH), Ordering::Relaxed);
}
pub fn dst_enabled() -> bool {
    DST_ENABLED.load(Ordering::Relaxed)
}
pub fn set_dst_enabled(on: bool) {
    DST_ENABLED.store(on, Ordering::Relaxed);
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The year containing day number `z` (days since 1970-01-01).
fn year_of_days(z: i64) -> i64 {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    yoe + era * 400 + (m <= 2) as i64
}

/// The last Sunday of a 31-day month, as a day number.
fn last_sunday(y: i64, m: i64) -> i64 {
    let last = days_from_civil(y, m, 31);
    last - (last + 4).rem_euclid(7) // 1970-01-01 was a Thursday (4)
}

/// Whether EU/UK summer time is in effect at this UTC time.
fn eu_dst(unix: i64) -> bool {
    let y = year_of_days(unix.div_euclid(86_400));
    let start = last_sunday(y, 3) * 86_400 + 3600;
    let end = last_sunday(y, 10) * 86_400 + 3600;
    (start..end).contains(&unix)
}
const RESYNC: Duration = Duration::from_secs(3600);
/// Retry delay while no time has been obtained yet.
const RETRY: Duration = Duration::from_secs(15);
/// Seconds from 1900-01-01 (the NTP epoch) to 1970-01-01.
const NTP_TO_UNIX: u64 = 2_208_988_800;

/// Unix time (seconds) at the moment `Instant` was taken, once synced.
static SYNC: Mutex<Cell<Option<(u64, Instant)>>> = Mutex::new(Cell::new(None));

/// The local time as (hour, minute), or None until NTP has succeeded.
pub fn hour_minute() -> Option<(u8, u8)> {
    let (unix, at) = critical_section::with(|cs| SYNC.borrow(cs).get())?;
    let utc = unix as i64 + at.elapsed_sat().as_secs() as i64;
    let dst = dst_enabled() && eu_dst(utc);
    let now = utc + offset_qh() as i64 * 900 + if dst { 3600 } else { 0 };
    let day_secs = now.rem_euclid(86_400);
    Some(((day_secs / 3600) as u8, (day_secs / 60 % 60) as u8))
}

/// Ask one server; returns Unix seconds and the Instant they were valid at.
async fn query(socket: &UdpSocket<'_>, server: IpAddress) -> Option<(u64, Instant)> {
    // LI 0, version 4, mode 3 (client); the rest zero.
    let mut req = [0u8; 48];
    req[0] = 0x23;
    socket.send_to(&req, (server, 123)).await.ok()?;
    let mut resp = [0u8; 48];
    let (n, meta) = with_timeout(Duration::from_secs(2), socket.recv_from(&mut resp)).await.ok()?.ok()?;
    let at = Instant::now();
    // Mode 4 (server), stratum 1-15 (0 = kiss-o'-death / unsynchronised),
    // and from the server we asked.
    if n < 48 || resp[0] & 7 != 4 || resp[1] == 0 || resp[1] > 15 || meta.endpoint.addr != server {
        return None;
    }
    let mut secs = u32::from_be_bytes([resp[40], resp[41], resp[42], resp[43]]) as u64;
    if secs == 0 {
        return None;
    }
    // The 32-bit field wraps in 2036: a small value then means era 1.
    if secs < NTP_TO_UNIX {
        secs += 1 << 32;
    }
    Some((secs - NTP_TO_UNIX, at))
}

async fn sync_once(stack: Stack<'static>) -> bool {
    let Ok(addrs) = stack.dns_query(SERVER, DnsQueryType::A).await else {
        return false;
    };
    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut rx = [0u8; 64];
    let mut tx = [0u8; 64];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    if socket.bind(0).is_err() {
        return false;
    }
    // The pool returns several addresses; try each in turn.
    for &addr in addrs.iter() {
        if let Some(t) = query(&socket, addr).await {
            critical_section::with(|cs| SYNC.borrow(cs).set(Some(t)));
            return true;
        }
    }
    false
}

#[embassy_executor::task]
pub async fn ntp_task(stack: Stack<'static>) -> ! {
    loop {
        stack.wait_config_up().await;
        let ok = sync_once(stack).await;
        Timer::after(if ok { RESYNC } else { RETRY }).await;
    }
}
