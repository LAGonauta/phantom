use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Cursor, Read};

pub const UNCONNECTED_PING_ID: u8 = 0x01;
pub const UNCONNECTED_PONG_ID: u8 = 0x1C;

#[derive(Debug, Clone)]
pub struct UnconnectedPing {
    pub ping_time: [u8; 8],
    pub id: [u8; 8],
    pub magic: [u8; 16],
    pub pong: PongData,
}

#[derive(Debug, Clone, Default)]
pub struct PongData {
    pub edition: String,
    pub motd: String,
    pub protocol_version: String,
    pub version: String,
    pub players: String,
    pub max_players: String,
    pub server_id: String,
    pub submotd: String,
    pub game_type: String,
    pub nintendo_limited: String,
    pub port4: String,
    pub port6: String,
}

// Try to parse Unconnected Ping, return Some(unconnected) if looks valid
pub fn read_unconnected_ping(buf: &[u8]) -> Result<Option<UnconnectedPing>, anyhow::Error> {
    if buf.len() < 1 {
        return Ok(None);
    }

    let mut cursor = Cursor::new(buf);
    let id = cursor.read_u8()?;
    if id != UNCONNECTED_PING_ID && id != UNCONNECTED_PONG_ID {
        return Ok(None);
    }

    // For ping or pong, attempt to parse fields
    // Skip id for consistency
    // Ensure enough bytes
    if cursor.get_ref().len() < 1 + 8 + 8 + 16 + 2 {
        return Ok(None);
    }

    // Move cursor back to start to read fully
    cursor.set_position(1);

    let mut ping_time = [0u8; 8];
    cursor.read_exact(&mut ping_time)?;
    let mut idb = [0u8; 8];
    cursor.read_exact(&mut idb)?;
    let mut magic = [0u8; 16];
    cursor.read_exact(&mut magic)?;

    let len = cursor.read_u16::<BigEndian>()? as usize;
    let mut pong_bytes = vec![0u8; len];
    cursor.read_exact(&mut pong_bytes)?;
    let pong_str = String::from_utf8_lossy(&pong_bytes).to_string();

    let pong = read_pong(&pong_str);

    Ok(Some(UnconnectedPing {
        ping_time,
        id: idb,
        magic,
        pong,
    }))
}

fn read_pong(raw: &str) -> PongData {
    let parts: Vec<&str> = raw.split(';').collect();
    let mut p = PongData::default();

    if parts.len() > 0 { p.edition = parts[0].to_string(); }
    if parts.len() > 1 { p.motd = parts[1].to_string(); }
    if parts.len() > 2 { p.protocol_version = parts[2].to_string(); }
    if parts.len() > 3 { p.version = parts[3].to_string(); }
    if parts.len() > 4 { p.players = parts[4].to_string(); }
    if parts.len() > 5 { p.max_players = parts[5].to_string(); }
    if parts.len() > 6 { p.server_id = parts[6].to_string(); }
    if parts.len() > 7 { p.submotd = parts[7].to_string(); }
    if parts.len() > 8 { p.game_type = parts[8].to_string(); }
    if parts.len() > 9 { p.nintendo_limited = parts[9].to_string(); }
    if parts.len() > 10 { p.port4 = parts[10].to_string(); }
    if parts.len() > 11 { p.port6 = parts[11].to_string(); }

    p
}

pub fn build_unconnected_pong(p: &UnconnectedPing) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(UNCONNECTED_PONG_ID);
    out.extend_from_slice(&p.ping_time);
    out.extend_from_slice(&p.id);
    out.extend_from_slice(&p.magic);

    let pong_str = write_pong(&p.pong);
    let mut len_buf = Vec::new();
    len_buf.write_u16::<BigEndian>(pong_str.len() as u16).unwrap();
    out.extend_from_slice(&len_buf);
    out.extend_from_slice(pong_str.as_bytes());
    out
}

fn write_pong(p: &PongData) -> String {
    let mut fields = vec![
        p.edition.clone(),
        p.motd.clone(),
        p.protocol_version.clone(),
        p.version.clone(),
        p.players.clone(),
        p.max_players.clone(),
        p.server_id.clone(),
        p.submotd.clone(),
        p.game_type.clone(),
        p.nintendo_limited.clone(),
        p.port4.clone(),
        p.port6.clone(),
    ];

    // Trim trailing empty fields
    while let Some(last) = fields.last() {
        if last.is_empty() {
            fields.pop();
        } else {
            break;
        }
    }

    format!("{};", fields.join(";"))
}
