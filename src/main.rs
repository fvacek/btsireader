use std::collections::HashMap;

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;
use futures::stream::StreamExt;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

const ADDR: &str = "CC:DA:B5:74:95:82";

// Card readout service  bd510011-…
const CARD_STATE_UUID: &str = "bd510012-6aec-4628-a146-f3e95bc49e62"; // notify: insert/remove
const CARD_DATA_UUID:  &str = "bd510013-6aec-4628-a146-f3e95bc49e62"; // notify: readout data

// Message IDs (little-endian in wire format)
const MSG_WRAPPER:       u16 = 0xA101; // segmented transport wrapper
const MSG_CARD_STATE:    u16 = 0x1101; // card inserted / removed
const MSG_CARD_MINIMAL:  u16 = 0x1102; // card data — number + punches only
const MSG_CARD_COMPLETE: u16 = 0x1103; // card data — full (incl. owner)

// ── Wrapper reassembler ──────────────────────────────────────────────────────
//
// Large messages are split across BLE notifications as wrapper packets:
//   First  (flag=0x01): 4-byte total_len, then first segment
//   Middle (flag=0x00): segment continuation
//   Last   (flag=0x02): final segment; reassembled buffer should equal total_len

#[derive(Default)]
struct Reassembler {
    buf: Vec<u8>,
    expected: usize,
}

impl Reassembler {
    /// Feed one raw notification value.
    /// Returns the fully reassembled message bytes when the last segment arrives,
    /// or None while still accumulating.
    fn feed(&mut self, raw: &[u8]) -> Option<Vec<u8>> {
        if raw.len() < 4 {
            return None;
        }
        let msg_id = u16::from_le_bytes([raw[0], raw[1]]);
        let plen   = u16::from_le_bytes([raw[2], raw[3]]) as usize;

        // Non-wrapped message — return as-is
        if msg_id != MSG_WRAPPER {
            return Some(raw[..4 + plen.min(raw.len().saturating_sub(4))].to_vec());
        }

        let payload = raw.get(4..4 + plen)?;
        let flag    = *payload.first()?;

        match flag {
            0x01 => {
                // First packet: next 4 bytes = total reassembled length
                if payload.len() < 5 {
                    return None;
                }
                self.expected = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]) as usize;
                self.buf.clear();
                self.buf.extend_from_slice(&payload[5..]);
                None
            }
            0x00 => {
                // Continuation
                self.buf.extend_from_slice(&payload[1..]);
                None
            }
            0x02 => {
                // Last packet
                self.buf.extend_from_slice(&payload[1..]);
                if self.buf.len() == self.expected {
                    Some(std::mem::take(&mut self.buf))
                } else {
                    eprintln!(
                        "reassembly length mismatch: got {} expected {}",
                        self.buf.len(), self.expected
                    );
                    self.buf.clear();
                    None
                }
            }
            _ => None,
        }
    }
}

// ── Message dispatch ─────────────────────────────────────────────────────────

fn handle_message(data: &[u8]) {
    if data.len() < 4 {
        return;
    }
    let msg_id  = u16::from_le_bytes([data[0], data[1]]);
    let plen    = u16::from_le_bytes([data[2], data[3]]) as usize;
    let Some(payload) = data.get(4..4 + plen) else {
        eprintln!("message truncated (id=0x{:04X})", msg_id);
        return;
    };

    match msg_id {
        MSG_CARD_STATE    => handle_card_state(payload),
        MSG_CARD_MINIMAL |
        MSG_CARD_COMPLETE => handle_card_readout(payload),
        other             => println!("[unknown msg 0x{:04X}, {} payload bytes]", other, plen),
    }
}

// ── CardStateChange (0x1101) ─────────────────────────────────────────────────
//
// Payload: card_number u32 | state u8 (1=In, 0=Out) | code_number u16

fn handle_card_state(p: &[u8]) {
    if p.len() < 7 {
        return;
    }
    let card  = u32::from_le_bytes(p[0..4].try_into().unwrap());
    let state = p[4];
    let code  = u16::from_le_bytes([p[5], p[6]]);
    println!(
        "Card {:>8}  station {:>3}  {}",
        card, code,
        if state == 1 { "INSERTED" } else { "REMOVED" }
    );
}

// ── CardDataReadout (0x1102 / 0x1103) ────────────────────────────────────────
//
// Payload: card_number u32 | card_family u8 | punch_count u16 | punches…
// Each punch (8 bytes): control_info u8 | punch_type u8 | code u16 | time_ms u32

fn handle_card_readout(p: &[u8]) {
    if p.len() < 7 {
        return;
    }
    let card_number = u32::from_le_bytes(p[0..4].try_into().unwrap());
    let card_family = p[4];
    let punch_count = u16::from_le_bytes([p[5], p[6]]) as usize;

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!(" Card {:>8}   family: {}", card_number, family_name(card_family));
    println!("───────────────────────────────────────────────");

    let punches_end = 7 + punch_count * 8;
    if p.len() < punches_end {
        println!(" [truncated punch data]");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        return;
    }

    for i in 0..punch_count {
        let o     = 7 + i * 8;
        let ptype = p[o + 1];
        let code  = u16::from_le_bytes([p[o + 2], p[o + 3]]);
        let t_ms  = u32::from_le_bytes(p[o + 4..o + 8].try_into().unwrap());
        println!(
            " {:>2}. {:12}  ctrl {:>3}  {}",
            i + 1, punch_type_name(ptype), code, format_time(t_ms)
        );
    }

    // Owner data: 1 byte charset + NUL-terminated string (field separator = ';')
    // Field order for SI-Card8/9: first_name ; last_name
    if p.len() > punches_end + 1 {
        let _charset   = p[punches_end]; // 1 = ISO-8859-1, 17 = GB2312
        let name_bytes = &p[punches_end + 1..];
        let nul        = name_bytes.iter().position(|&b| b == 0).unwrap_or(name_bytes.len());
        if nul > 0 {
            let s = String::from_utf8_lossy(&name_bytes[..nul]);
            let parts: Vec<&str> = s.splitn(3, ';').collect();
            let first = parts.first().copied().unwrap_or("").trim();
            let last  = parts.get(1).copied().unwrap_or("").trim();
            if !first.is_empty() || !last.is_empty() {
                println!("───────────────────────────────────────────────");
                println!(" Owner: {} {}", first, last);
            }
        }
    }

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn format_time(ms: u32) -> String {
    const DAYS: &[&str] = &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let day  = (ms / 86_400_000) as usize % 7;
    let tod  = ms % 86_400_000;
    let h    = tod / 3_600_000;
    let m    = (tod % 3_600_000) / 60_000;
    let s    = (tod % 60_000) / 1_000;
    let ms_r = tod % 1_000;
    format!("{} {:02}:{:02}:{:02}.{:03}", DAYS[day], h, m, s, ms_r)
}

fn family_name(f: u8) -> &'static str {
    match f {
        1  => "SI-Card9",
        2  => "SI-Card8",
        4  => "SI-pCard",
        15 => "SIAC",
        _  => "unknown",
    }
}

fn punch_type_name(t: u8) -> &'static str {
    match t {
        0 => "Undefined",
        1 => "Clear",
        2 => "Check",
        3 => "Start",
        4 => "StartReserve",
        5 => "Finish",
        6 => "FinishReserve",
        7 => "Control",
        _ => "?",
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let card_state_uuid = Uuid::parse_str(CARD_STATE_UUID)?;
    let card_data_uuid  = Uuid::parse_str(CARD_DATA_UUID)?;

    let manager  = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let central  = adapters.into_iter().next().expect("no BLE adapter");

    central.start_scan(ScanFilter::default()).await?;
    sleep(Duration::from_secs(5)).await;

    let device = central.peripherals().await?
        .into_iter()
        .find(|p| p.address().to_string().eq_ignore_ascii_case(ADDR))
        .ok_or_else(|| format!("device {} not found", ADDR))?;

    device.connect().await?;
    device.discover_services().await?;
    println!("Connected to SportIdent Reader BT\n");

    let chars = device.characteristics();
    for uuid in [&card_state_uuid, &card_data_uuid] {
        match chars.iter().find(|c| &c.uuid == uuid) {
            Some(c) => device.subscribe(c).await?,
            None    => eprintln!("characteristic {} not found", uuid),
        }
    }

    println!("Waiting for card insertions…\n");

    // Each characteristic gets its own reassembler in case both fire concurrently.
    let mut reassemblers: HashMap<Uuid, Reassembler> = HashMap::new();

    let mut stream = device.notifications().await?;
    while let Some(event) = stream.next().await {
        let r = reassemblers.entry(event.uuid).or_default();
        if let Some(msg) = r.feed(&event.value) {
            handle_message(&msg);
        }
    }

    Ok(())
}
