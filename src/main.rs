use std::collections::HashMap;

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;
use futures::stream::StreamExt;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

const ADDR: &str = "CC:DA:B5:74:95:82";

const CARD_STATE_UUID: &str = "bd510012-6aec-4628-a146-f3e95bc49e62";
const CARD_DATA_UUID:  &str = "bd510013-6aec-4628-a146-f3e95bc49e62";

const MSG_WRAPPER:       u16 = 0xA101;
const MSG_CARD_STATE:    u16 = 0x1101;
const MSG_CARD_MINIMAL:  u16 = 0x1102;
const MSG_CARD_COMPLETE: u16 = 0x1103;

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub struct Punch {
    pub control_info: u8,
    pub punch_type:   u8,   // 1=Clear 2=Check 3=Start 5=Finish 7=Control …
    pub control_code: u16,
    pub time_ms:      u32,  // milliseconds since Sunday 00:00:00.000
}

#[derive(Debug, PartialEq)]
pub struct CardReadout {
    pub card_number: u32,
    pub card_family: u8,   // 1=SI-Card9 2=SI-Card8 4=SI-pCard 15=SIAC
    pub punches:     Vec<Punch>,
    pub owner:       Option<String>, // "First Last", None when not registered
}

#[derive(Debug, PartialEq)]
pub struct CardState {
    pub card_number: u32,
    pub state:       u8,   // 1 = inserted, 0 = removed
    pub code_number: u16,
}

// ── Wrapper-message reassembler ───────────────────────────────────────────────
//
// Large messages are split across BLE notifications as 0xA101 wrapper packets:
//   First  (flag 0x01):  u32 total_len  +  first segment bytes
//   Middle (flag 0x00):  segment bytes
//   Last   (flag 0x02):  final segment bytes; buffer must equal total_len

#[derive(Default)]
struct Reassembler {
    buf:      Vec<u8>,
    expected: usize,
}

impl Reassembler {
    fn feed(&mut self, raw: &[u8]) -> Option<Vec<u8>> {
        if raw.len() < 4 {
            return None;
        }
        let msg_id = u16::from_le_bytes([raw[0], raw[1]]);
        let plen   = u16::from_le_bytes([raw[2], raw[3]]) as usize;

        if msg_id != MSG_WRAPPER {
            return Some(raw[..4 + plen.min(raw.len().saturating_sub(4))].to_vec());
        }

        let payload = raw.get(4..4 + plen)?;
        let flag    = *payload.first()?;

        match flag {
            0x01 => {
                if payload.len() < 5 { return None; }
                self.expected = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]) as usize;
                self.buf.clear();
                self.buf.extend_from_slice(&payload[5..]);
                None
            }
            0x00 => {
                self.buf.extend_from_slice(&payload[1..]);
                None
            }
            0x02 => {
                self.buf.extend_from_slice(&payload[1..]);
                if self.buf.len() == self.expected {
                    Some(std::mem::take(&mut self.buf))
                } else {
                    eprintln!("reassembly length mismatch: got {} expected {}", self.buf.len(), self.expected);
                    self.buf.clear();
                    None
                }
            }
            _ => None,
        }
    }
}

// ── Pure parsing functions ────────────────────────────────────────────────────

/// Parse a CardStateChange payload (7 bytes).
pub fn parse_card_state(p: &[u8]) -> Option<CardState> {
    if p.len() < 7 { return None; }
    Some(CardState {
        card_number: u32::from_le_bytes(p[0..4].try_into().unwrap()),
        state:       p[4],
        code_number: u16::from_le_bytes([p[5], p[6]]),
    })
}

/// Parse a CardDataReadout payload (minimal or complete).
///
/// Layout: card_number u32 | card_family u8 | punch_count u16 | punches…
/// Each punch: control_info u8 | punch_type u8 | control_code u16 | time_ms u32
/// Optional trailing owner block: charset u8 | NUL-terminated "first;last;…" string
pub fn parse_card_readout(p: &[u8]) -> Option<CardReadout> {
    if p.len() < 7 { return None; }

    let card_number = u32::from_le_bytes(p[0..4].try_into().unwrap());
    let card_family = p[4];
    let punch_count = u16::from_le_bytes([p[5], p[6]]) as usize;
    let punches_end = 7 + punch_count * 8;

    if p.len() < punches_end { return None; }

    let mut punches = Vec::with_capacity(punch_count);
    for i in 0..punch_count {
        let o = 7 + i * 8;
        punches.push(Punch {
            control_info: p[o],
            punch_type:   p[o + 1],
            control_code: u16::from_le_bytes([p[o + 2], p[o + 3]]),
            time_ms:      u32::from_le_bytes(p[o + 4..o + 8].try_into().unwrap()),
        });
    }

    // Owner block is optional and only valid when a NUL terminator is present;
    // absent NUL means the area is uninitialised (card never had owner data written).
    let owner = if p.len() > punches_end + 1 {
        let _charset   = p[punches_end]; // 1 = ISO-8859-1, 17 = GB2312
        let name_bytes = &p[punches_end + 1..];
        name_bytes.iter().position(|&b| b == 0).and_then(|nul| {
            if nul == 0 { return None; }
            let s     = String::from_utf8_lossy(&name_bytes[..nul]);
            let mut f = s.split(';');
            let first = f.next().unwrap_or("").trim();
            let last  = f.next().unwrap_or("").trim();
            if !first.is_empty() || !last.is_empty() {
                Some(format!("{} {}", first, last).trim().to_string())
            } else {
                None
            }
        })
    } else {
        None
    };

    Some(CardReadout { card_number, card_family, punches, owner })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Format a week-based millisecond timestamp as "Day HH:MM:SS.mmm".
pub fn format_time(ms: u32) -> String {
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

// ── Display ───────────────────────────────────────────────────────────────────

fn print_card_state(s: &CardState) {
    println!(
        "Card {:>8}  station {:>3}  {}",
        s.card_number, s.code_number,
        if s.state == 1 { "INSERTED" } else { "REMOVED" }
    );
}

fn print_card_readout(r: &CardReadout) {
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!(" Card {:>8}   family: {}", r.card_number, family_name(r.card_family));
    println!("───────────────────────────────────────────────");
    for (i, p) in r.punches.iter().enumerate() {
        println!(
            " {:>2}. {:12}  ctrl {:>3}  {}",
            i + 1, punch_type_name(p.punch_type), p.control_code, format_time(p.time_ms)
        );
    }
    if let Some(owner) = &r.owner {
        println!("───────────────────────────────────────────────");
        println!(" Owner: {}", owner);
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}

// ── Message dispatch ──────────────────────────────────────────────────────────

fn handle_message(data: &[u8]) {
    if data.len() < 4 { return; }
    let msg_id  = u16::from_le_bytes([data[0], data[1]]);
    let plen    = u16::from_le_bytes([data[2], data[3]]) as usize;
    let Some(payload) = data.get(4..4 + plen) else {
        eprintln!("message truncated (id=0x{:04X})", msg_id);
        return;
    };
    match msg_id {
        MSG_CARD_STATE              => {
            if let Some(s) = parse_card_state(payload)    { print_card_state(&s);   }
        }
        MSG_CARD_MINIMAL |
        MSG_CARD_COMPLETE           => {
            if let Some(r) = parse_card_readout(payload)  { print_card_readout(&r); }
            else { eprintln!("truncated card readout payload"); }
        }
        other                       => println!("[unknown msg 0x{:04X}, {} payload bytes]", other, plen),
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Raw BLE notification bytes captured from a real SI-Card9 readout.
    // Wrapper message 0xA101, split across two notifications (total 260 reassembled bytes).
    //
    // Inner message: 0x1103 CardDataReadoutCompleteData
    //   card_number = 1504639  card_family = 1 (SI-Card9)  punch_count = 28
    #[rustfmt::skip]
    const NOTIF_1: &[u8] = &[
        // ── wrapper header ──────────────────────────────────────────
        0x01, 0xA1,             // msg_id = 0xA101
        0xF4, 0x00,             // payload_len = 244
        // ── wrapper payload ─────────────────────────────────────────
        0x01,                   // packet_flag = First
        0x04, 0x01, 0x00, 0x00, // total_message_length = 260
        // ── inner message header (first 4 of 260 reassembled bytes) ─
        0x03, 0x11,             // msg_id = 0x1103 (CardDataReadoutCompleteData)
        0x00, 0x01,             // payload_len = 256
        // ── inner payload ───────────────────────────────────────────
        0x7F, 0xF5, 0x16, 0x00, // card_number = 1 504 639 (LE)
        0x01,                   // card_family = 1 (SI-Card9)
        0x1C, 0x00,             // punch_count = 28
        // punch  1 – Clear   ctrl   3   Wed 17:25:11.000
        0x00, 0x01, 0x03, 0x00, 0xD8, 0xF8, 0x2F, 0x13,
        // punch  2 – Check   ctrl   3   Wed 17:25:11.000
        0x00, 0x02, 0x03, 0x00, 0xD8, 0xF8, 0x2F, 0x13,
        // punch  3 – Start   ctrl   4   Wed 17:26:44.000
        0x00, 0x03, 0x04, 0x00, 0x20, 0x64, 0x31, 0x13,
        // punch  4 – Finish  ctrl   7   Wed 18:22:50.000
        0x00, 0x05, 0x07, 0x00, 0x90, 0xC0, 0x64, 0x13,
        // punch  5 – Control ctrl  48   Wed 17:29:44.000
        0x00, 0x07, 0x30, 0x00, 0x40, 0x23, 0x34, 0x13,
        // punch  6 – Control ctrl  79   Wed 17:30:27.000
        0x00, 0x07, 0x4F, 0x00, 0x38, 0xCB, 0x34, 0x13,
        // punch  7 – Control ctrl  67   Wed 17:32:26.000
        0x00, 0x07, 0x43, 0x00, 0x10, 0x9C, 0x36, 0x13,
        // punch  8 – Control ctrl  33   Wed 17:33:57.000
        0x00, 0x07, 0x21, 0x00, 0x88, 0xFF, 0x37, 0x13,
        // punch  9 – Control ctrl  56   Wed 17:34:51.000
        0x00, 0x07, 0x38, 0x00, 0x78, 0xD2, 0x38, 0x13,
        // punch 10 – Control ctrl  52   Wed 17:36:43.000
        0x00, 0x07, 0x34, 0x00, 0xF8, 0x87, 0x3A, 0x13,
        // punch 11 – Control ctrl  65   Wed 17:39:04.000
        0x00, 0x07, 0x41, 0x00, 0xC0, 0xAE, 0x3C, 0x13,
        // punch 12 – Control ctrl  69   Wed 17:40:30.000
        0x00, 0x07, 0x45, 0x00, 0xB0, 0xFE, 0x3D, 0x13,
        // punch 13 – Control ctrl  70   Wed 17:44:26.000
        0x00, 0x07, 0x46, 0x00, 0x90, 0x98, 0x41, 0x13,
        // punch 14 – Control ctrl  76   Wed 17:46:45.000
        0x00, 0x07, 0x4C, 0x00, 0x88, 0xB7, 0x43, 0x13,
        // punch 15 – Control ctrl  80   Wed 17:48:46.000
        0x00, 0x07, 0x50, 0x00, 0x30, 0x90, 0x45, 0x13,
        // punch 16 – Control ctrl  49   Wed 17:51:18.000
        0x00, 0x07, 0x31, 0x00, 0xF0, 0xE1, 0x47, 0x13,
        // punch 17 – Control ctrl  77   Wed 17:54:09.000
        0x00, 0x07, 0x4D, 0x00, 0xE8, 0x7D, 0x4A, 0x13,
        // punch 18 – Control ctrl  75   Wed 17:56:14.000
        0x00, 0x07, 0x4B, 0x00, 0x30, 0x66, 0x4C, 0x13,
        // punch 19 – Control ctrl  74   Wed 18:00:17.000
        0x00, 0x07, 0x4A, 0x00, 0x68, 0x1B, 0x50, 0x13,
        // punch 20 – Control ctrl  44   Wed 18:06:19.000
        0x00, 0x07, 0x2C, 0x00, 0x78, 0xA1, 0x55, 0x13,
        // punch 21 – Control ctrl  39   Wed 18:08:18.000
        0x00, 0x07, 0x27, 0x00, 0x50, 0x72, 0x57, 0x13,
        // punch 22 – Control ctrl  83   Wed 18:09:04.000
        0x00, 0x07, 0x53, 0x00, 0x00, 0x26, 0x58, 0x13,
        // punch 23 – Control ctrl  37   Wed 18:10:33.000
        0x00, 0x07, 0x25, 0x00, 0xA8, 0x81, 0x59, 0x13,
        // punch 24 – Control ctrl  45   Wed 18:13:27.000
        0x00, 0x07, 0x2D, 0x00, 0x58, 0x29, 0x5C, 0x13,
        // punch 25 – Control ctrl  43   Wed 18:17:19.000
        0x00, 0x07, 0x2B, 0x00, 0x98, 0xB3, 0x5F, 0x13,
        // punch 26 – Control ctrl  84   Wed 18:20:27.000
        0x00, 0x07, 0x54, 0x00, 0xF8, 0x91, 0x62, 0x13,
        // punch 27 – Control ctrl  40   Wed 18:21:21.000
        0x00, 0x07, 0x28, 0x00, 0xE8, 0x64, 0x63, 0x13,
        // punch 28 – Control ctrl 100   Wed 18:22:39.000
        0x00, 0x07, 0x64, 0x00, 0x98, 0x95, 0x64, 0x13,
        // ── owner block (first 4 of 25 bytes; rest in NOTIF_2) ──────
        0x01,                   // charset = 1 (ISO-8859-1)
        0x3B, 0x3B, 0xEE,       // ";;î" — empty first+last name, junk beyond
    ];

    #[rustfmt::skip]
    const NOTIF_2: &[u8] = &[
        // ── wrapper header ──────────────────────────────────────────
        0x01, 0xA1,             // msg_id = 0xA101
        0x16, 0x00,             // payload_len = 22
        // ── wrapper payload ─────────────────────────────────────────
        0x02,                   // packet_flag = Last
        // remaining 21 bytes of owner block (contains NUL + zero-padding)
        0xEE, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    // ── Reassembler ──────────────────────────────────────────────────────────

    #[test]
    fn reassembler_passes_through_non_wrapper() {
        // A non-wrapper message must be returned unchanged.
        let msg: &[u8] = &[0x01, 0x11, 0x07, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];
        let out = Reassembler::default().feed(msg);
        assert_eq!(out, Some(msg.to_vec()));
    }

    #[test]
    fn reassembler_first_packet_returns_none() {
        let mut r = Reassembler::default();
        assert!(r.feed(NOTIF_1).is_none());
    }

    #[test]
    fn reassembler_two_segments_produce_correct_message() {
        let msg = reassemble();
        // 4-byte header + 256-byte payload = 260 reassembled bytes
        assert_eq!(msg.len(), 260);
        assert_eq!(u16::from_le_bytes([msg[0], msg[1]]), MSG_CARD_COMPLETE);
        assert_eq!(u16::from_le_bytes([msg[2], msg[3]]), 256);
    }

    // ── format_time ──────────────────────────────────────────────────────────

    #[test]
    fn format_time_sunday_midnight() {
        assert_eq!(format_time(0), "Sun 00:00:00.000");
    }

    #[test]
    fn format_time_known_punches() {
        // Values computed from the captured card data.
        assert_eq!(format_time(321_911_000), "Wed 17:25:11.000"); // Clear / Check
        assert_eq!(format_time(322_004_000), "Wed 17:26:44.000"); // Start
        assert_eq!(format_time(325_370_000), "Wed 18:22:50.000"); // Finish
        assert_eq!(format_time(322_184_000), "Wed 17:29:44.000"); // ctrl 48
        assert_eq!(format_time(325_359_000), "Wed 18:22:39.000"); // ctrl 100 (last)
    }

    // ── parse_card_state ─────────────────────────────────────────────────────

    #[test]
    fn parse_card_state_inserted() {
        // card 1504639 inserted at station 7
        let payload: &[u8] = &[0x7F, 0xF5, 0x16, 0x00, 0x01, 0x07, 0x00];
        let s = parse_card_state(payload).unwrap();
        assert_eq!(s.card_number, 1_504_639);
        assert_eq!(s.state, 1);
        assert_eq!(s.code_number, 7);
    }

    #[test]
    fn parse_card_state_removed() {
        let payload: &[u8] = &[0x7F, 0xF5, 0x16, 0x00, 0x00, 0x07, 0x00];
        assert_eq!(parse_card_state(payload).unwrap().state, 0);
    }

    #[test]
    fn parse_card_state_too_short_returns_none() {
        assert!(parse_card_state(&[0x01, 0x02, 0x03]).is_none());
    }

    // ── parse_card_readout ───────────────────────────────────────────────────

    #[test]
    fn parse_card_readout_metadata() {
        let r = parse_readout();
        assert_eq!(r.card_number, 1_504_639);
        assert_eq!(r.card_family, 1); // SI-Card9
        assert_eq!(r.punches.len(), 28);
    }

    #[test]
    fn parse_card_readout_special_punches() {
        let r = parse_readout();

        let clear = &r.punches[0];
        assert_eq!(clear.punch_type,   1); // Clear
        assert_eq!(clear.control_code, 3);
        assert_eq!(clear.time_ms, 321_911_000);

        let check = &r.punches[1];
        assert_eq!(check.punch_type,   2); // Check
        assert_eq!(check.control_code, 3);
        assert_eq!(check.time_ms, 321_911_000);

        let start = &r.punches[2];
        assert_eq!(start.punch_type,   3); // Start
        assert_eq!(start.control_code, 4);
        assert_eq!(start.time_ms, 322_004_000);

        let finish = &r.punches[3];
        assert_eq!(finish.punch_type,   5); // Finish
        assert_eq!(finish.control_code, 7);
        assert_eq!(finish.time_ms, 325_370_000);
    }

    #[test]
    fn parse_card_readout_control_codes_in_order() {
        let r = parse_readout();
        let codes: Vec<u16> = r.punches[4..].iter().map(|p| p.control_code).collect();
        assert_eq!(
            codes,
            vec![48, 79, 67, 33, 56, 52, 65, 69, 70, 76, 80, 49, 77, 75, 74, 44, 39, 83, 37, 45, 43, 84, 40, 100]
        );
    }

    #[test]
    fn parse_card_readout_all_controls_have_type_7() {
        let r = parse_readout();
        for p in &r.punches[4..] {
            assert_eq!(p.punch_type, 7, "expected Control (7), got {} for ctrl {}", p.punch_type, p.control_code);
        }
    }

    #[test]
    fn parse_card_readout_last_control_time() {
        let r = parse_readout();
        let last = r.punches.last().unwrap();
        assert_eq!(last.control_code, 100);
        assert_eq!(last.time_ms, 325_359_000);
        assert_eq!(format_time(last.time_ms), "Wed 18:22:39.000");
    }

    // ── owner data ───────────────────────────────────────────────────────────

    #[test]
    fn owner_is_none_when_no_nul_in_owner_area() {
        // Uninitialised owner area (all 0xFF, no NUL terminator) → no owner.
        let mut p = vec![0x01u8, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]; // header, 0 punches
        p.push(0x01); // charset
        p.extend_from_slice(&[0xFF; 24]);
        assert_eq!(parse_card_readout(&p).unwrap().owner, None);
    }

    #[test]
    fn owner_is_none_when_both_name_fields_empty() {
        // ";;garbage\0" – separator present but both name fields are empty.
        let mut p = vec![0x01u8, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00];
        p.push(0x01); // charset
        p.extend_from_slice(b";;\xEE\xEE\x00"); // ";;îî\0"
        assert_eq!(parse_card_readout(&p).unwrap().owner, None);
    }

    #[test]
    fn owner_parsed_correctly_when_present() {
        let mut p = vec![0x01u8, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00];
        p.push(0x01); // charset = ISO-8859-1
        p.extend_from_slice(b"John;Smith\x00"); // "first;last\0"
        p.extend_from_slice(&[0x00; 13]); // padding
        assert_eq!(parse_card_readout(&p).unwrap().owner, Some("John Smith".to_string()));
    }

    #[test]
    fn owner_is_none_for_captured_card_no_owner_registered() {
        assert_eq!(parse_readout().owner, None);
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Feed both captured notifications through a fresh Reassembler,
    /// then parse the resulting inner payload.
    fn parse_readout() -> CardReadout {
        let msg = reassemble();
        parse_card_readout(&msg[4..]).expect("parse_card_readout failed")
    }

    /// Feed both captured notifications through a fresh Reassembler.
    fn reassemble() -> Vec<u8> {
        let mut r = Reassembler::default();
        assert!(r.feed(NOTIF_1).is_none(), "first notification must not complete the message");
        r.feed(NOTIF_2).expect("second notification must complete the message")
    }
}
