//! MySQL binary result 的叶子编解码；状态机仍由内嵌 Lua 协议实现。
use cosmos::userdata::{LuaBytes, Null};
#[derive(cosmos::PushLua)]
pub(super) enum Cell {
    Number(f64),
    Text(String),
    Bytes(Vec<u8>),
    Null(Null),
}
fn take<'a>(data: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
    if data.len() < n {
        return Err("truncated MySQL binary value".into());
    }
    let (head, tail) = data.split_at(n);
    *data = tail;
    Ok(head)
}
fn unsigned(data: &mut &[u8], n: usize) -> Result<u64, String> {
    let mut bytes = [0u8; 8];
    bytes[..n].copy_from_slice(take(data, n)?);
    Ok(u64::from_le_bytes(bytes))
}
fn lenenc(data: &mut &[u8]) -> Result<usize, String> {
    let first = unsigned(data, 1)?;
    let n = match first {
        0..=250 => first,
        252 => unsigned(data, 2)?,
        253 => unsigned(data, 3)?,
        254 => unsigned(data, 8)?,
        _ => return Err("invalid MySQL length encoding".into()),
    };
    usize::try_from(n).map_err(|_| "MySQL binary length is out of range".into())
}
pub(super) fn decode(
    bytes: LuaBytes,
    kind: f64,
    flags: f64,
    position: f64,
) -> Result<(Cell, u32), String> {
    if bytes.len() > 64 * 1024 * 1024
        || !position.is_finite()
        || position.fract() != 0.0
        || position < 1.0
        || position > bytes.len() as f64 + 1.0
    {
        return Err("invalid MySQL binary row position or size".into());
    }
    if !kind.is_finite()
        || kind.fract() != 0.0
        || !(0.0..=255.0).contains(&kind)
        || !flags.is_finite()
        || flags.fract() != 0.0
        || !(0.0..=65535.0).contains(&flags)
    {
        return Err("invalid MySQL column type or flags".into());
    }
    let mut data = &bytes[position as usize - 1..];
    let before = data.len();
    let kind = kind as u8;
    let value = match kind {
        1 | 2 | 3 | 8 | 9 | 13 => {
            let n = match kind {
                1 => 1,
                2 | 13 => 2,
                3 | 9 => 4,
                _ => 8,
            };
            let raw = unsigned(&mut data, n)?;
            if flags as u16 & 32 != 0 {
                if kind == 8 || raw > 9_007_199_254_740_991 {
                    Cell::Text(raw.to_string())
                } else {
                    Cell::Number(raw as f64)
                }
            } else {
                let signed = if n == 8 {
                    raw as i64
                } else {
                    ((raw << (64 - n * 8)) as i64) >> (64 - n * 8)
                };
                if kind == 8 || signed.unsigned_abs() > 9_007_199_254_740_991 {
                    Cell::Text(signed.to_string())
                } else {
                    Cell::Number(signed as f64)
                }
            }
        }
        4 | 5 => {
            let n = if kind == 4 { 4 } else { 8 };
            let raw = unsigned(&mut data, n)?;
            let value = if kind == 4 {
                f32::from_bits(raw as u32) as f64
            } else {
                f64::from_bits(raw)
            };
            if !value.is_finite() {
                return Err("non-finite MySQL binary float".into());
            }
            Cell::Number(value)
        }
        6 => Cell::Null(Null),
        7 | 10 | 12 | 14 => {
            let len = unsigned(&mut data, 1)? as usize;
            if !matches!(len, 0 | 4 | 7 | 11) {
                return Err("invalid MySQL binary date length".into());
            }
            let date_only = kind == 10 || kind == 14;
            if date_only && len > 4 {
                return Err("invalid MySQL binary date payload".into());
            }
            let mut part = take(&mut data, len)?;
            let (year, month, day) = if len >= 4 {
                (
                    unsigned(&mut part, 2)?,
                    unsigned(&mut part, 1)?,
                    unsigned(&mut part, 1)?,
                )
            } else {
                (0, 0, 0)
            };
            let mut value = format!("{year:04}-{month:02}-{day:02}");
            if !date_only {
                let (hour, minute, second) = if len >= 7 {
                    (
                        unsigned(&mut part, 1)?,
                        unsigned(&mut part, 1)?,
                        unsigned(&mut part, 1)?,
                    )
                } else {
                    (0, 0, 0)
                };
                if hour > 23 || minute > 59 || second > 59 {
                    return Err("invalid MySQL binary time".into());
                }
                value.push_str(&format!(" {hour:02}:{minute:02}:{second:02}"));
                if len == 11 {
                    let micros = unsigned(&mut part, 4)?;
                    if micros > 999999 {
                        return Err("invalid MySQL binary microseconds".into());
                    }
                    value.push_str(&format!(".{micros:06}"));
                }
            }
            if year > 9999 || month > 12 || day > 31 {
                return Err("invalid MySQL binary date".into());
            }
            Cell::Text(value)
        }
        11 => {
            let len = unsigned(&mut data, 1)? as usize;
            if !matches!(len, 0 | 8 | 12) {
                return Err("invalid MySQL binary duration length".into());
            }
            if len == 0 {
                Cell::Text("00:00:00".into())
            } else {
                let mut part = take(&mut data, len)?;
                let sign = unsigned(&mut part, 1)?;
                let days = unsigned(&mut part, 4)?;
                let hour = unsigned(&mut part, 1)?;
                let minute = unsigned(&mut part, 1)?;
                let second = unsigned(&mut part, 1)?;
                if sign > 1 || hour > 23 || minute > 59 || second > 59 {
                    return Err("invalid MySQL binary duration".into());
                }
                let mut value = format!(
                    "{}{:02}:{minute:02}:{second:02}",
                    if sign == 1 { "-" } else { "" },
                    days * 24 + hour
                );
                if len == 12 {
                    let micros = unsigned(&mut part, 4)?;
                    if micros > 999999 {
                        return Err("invalid MySQL binary microseconds".into());
                    }
                    value.push_str(&format!(".{micros:06}"));
                }
                Cell::Text(value)
            }
        }
        0 | 15 | 16 | 245..=255 => {
            let len = lenenc(&mut data)?;
            Cell::Bytes(take(&mut data, len)?.to_vec())
        }
        _ => return Err("unsupported MySQL binary column type".into()),
    };
    Ok((value, (position as usize + before - data.len()) as u32))
}
