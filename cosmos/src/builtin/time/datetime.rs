//! 日期时间句柄保持一个 UTC 时刻与显示时区；本地时间的 DST 歧义明确报错。
use chrono::{
    DateTime, Datelike, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, Offset, SecondsFormat,
    TimeZone, Timelike, Utc,
};
use chrono_tz::Tz;

const MAX_INTEGER: f64 = 9_007_199_254_740_991.0;
#[derive(Clone, Copy)]
enum Zone {
    Fixed(FixedOffset),
    Named(Tz),
}
impl Zone {
    fn parse(name: Option<&str>) -> Result<Self, String> {
        let name = name.unwrap_or("UTC");
        if name.len() > 128 {
            return Err("invalid timezone".into());
        }
        if name == "Z" {
            return Ok(Self::Fixed(FixedOffset::east_opt(0).unwrap()));
        }
        if name.starts_with(['+', '-']) {
            let bytes = name.as_bytes();
            if bytes.len() != 6
                || bytes[3] != b':'
                || !bytes[1..3]
                    .iter()
                    .chain(&bytes[4..6])
                    .all(u8::is_ascii_digit)
            {
                return Err("fixed timezone must use +HH:MM or -HH:MM".into());
            }
            let hour = name[1..3].parse::<i32>().map_err(|_| "invalid timezone")?;
            let minute = name[4..6].parse::<i32>().map_err(|_| "invalid timezone")?;
            if hour > 23 || minute > 59 {
                return Err("timezone offset is out of range".into());
            }
            let seconds = (hour * 3600 + minute * 60) * if bytes[0] == b'-' { -1 } else { 1 };
            return Ok(Self::Fixed(
                FixedOffset::east_opt(seconds).ok_or("timezone offset is out of range")?,
            ));
        }
        name.parse::<Tz>()
            .map(Self::Named)
            .map_err(|_| "unknown IANA timezone".into())
    }
    fn name(self) -> String {
        match self {
            Self::Fixed(offset) => offset.to_string(),
            Self::Named(zone) => zone.to_string(),
        }
    }
    fn at(self, value: DateTime<Utc>) -> DateTime<FixedOffset> {
        match self {
            Self::Fixed(zone) => value.with_timezone(&zone),
            Self::Named(zone) => value.with_timezone(&zone).fixed_offset(),
        }
    }
    fn local(self, value: NaiveDateTime) -> Result<DateTime<Utc>, String> {
        self.resolve(value, "reject")
    }
    fn resolve(self, value: NaiveDateTime, ambiguity: &str) -> Result<DateTime<Utc>, String> {
        if !matches!(ambiguity, "reject" | "earliest" | "latest") {
            return Err("datetime ambiguity must be reject, earliest or latest".into());
        }
        let result = match self {
            Self::Fixed(zone) => zone
                .from_local_datetime(&value)
                .map(|d| d.with_timezone(&Utc)),
            Self::Named(zone) => zone
                .from_local_datetime(&value)
                .map(|d| d.with_timezone(&Utc)),
        };
        match result {
            LocalResult::Single(value) => Ok(value),
            LocalResult::Ambiguous(first, second) => match ambiguity {
                "earliest" => Ok(first.min(second)),
                "latest" => Ok(first.max(second)),
                _ => Err(
                    "ambiguous local datetime; supply an explicit UTC offset or ambiguity policy"
                        .into(),
                ),
            },
            LocalResult::None => Err("nonexistent or out-of-range local datetime".into()),
        }
    }
}
#[derive(Clone)]
pub(super) struct Datetime {
    utc: DateTime<Utc>,
    zone: Zone,
}
impl Datetime {
    fn checked(utc: DateTime<Utc>, zone: Zone) -> Result<Self, String> {
        let offset = match zone {
            Zone::Fixed(offset) => offset,
            Zone::Named(zone) => zone.offset_from_utc_datetime(&utc.naive_utc()).fix(),
        };
        let local = utc
            .naive_utc()
            .checked_add_signed(chrono::TimeDelta::seconds(offset.local_minus_utc() as i64))
            .ok_or("datetime is out of range")?;
        if !(1..=9999).contains(&utc.year()) || !(1..=9999).contains(&local.year()) {
            return Err(
                "datetime year must be in 1..=9999 in UTC and the selected timezone".into(),
            );
        }
        if utc.timestamp_subsec_nanos() >= 1_000_000_000 {
            return Err("leap seconds are not supported".into());
        }
        Ok(Self { utc, zone })
    }
}
#[derive(crate::FromLua)]
struct Components {
    year: f64,
    month: f64,
    day: f64,
    #[lua(default)]
    hour: Option<f64>,
    #[lua(default)]
    minute: Option<f64>,
    #[lua(default)]
    second: Option<f64>,
    #[lua(default)]
    millisecond: Option<f64>,
    #[lua(default)]
    timezone: Option<String>,
}
#[derive(crate::PushLua)]
struct Parts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millisecond: i64,
    nanosecond: i64,
    weekday: i64,
    ordinal: i64,
    iso_year: i64,
    iso_week: i64,
    days_in_month: i64,
    offset_seconds: i64,
    timezone: String,
}
fn integer(value: f64, min: f64, max: f64, name: &str) -> Result<i64, String> {
    if !value.is_finite() || value.fract() != 0.0 || !(min..=max).contains(&value) {
        return Err(format!("invalid datetime {name}"));
    }
    Ok(value as i64)
}
#[derive(Default, crate::FromLua)]
#[lua(default)]
struct Calendar {
    years: f64,
    months: f64,
    weeks: f64,
    days: f64,
    overflow: Option<String>,
    ambiguity: Option<String>,
}
fn month_days(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .unwrap()
        .pred_opt()
        .unwrap()
        .day()
}
#[crate::userdata(name = "time.datetime")]
impl Datetime {
    fn now(timezone: Option<String>) -> Result<Self, String> {
        Self::checked(
            std::time::SystemTime::now().into(),
            Zone::parse(timezone.as_deref())?,
        )
    }
    fn new(parts: Components) -> Result<Self, String> {
        let year = integer(parts.year, 1.0, 9999.0, "year")? as i32;
        let month = integer(parts.month, 1.0, 12.0, "month")? as u32;
        let day = integer(parts.day, 1.0, 31.0, "day")? as u32;
        let hour = integer(parts.hour.unwrap_or(0.0), 0.0, 23.0, "hour")? as u32;
        let minute = integer(parts.minute.unwrap_or(0.0), 0.0, 59.0, "minute")? as u32;
        let second = integer(parts.second.unwrap_or(0.0), 0.0, 59.0, "second")? as u32;
        let ms = integer(parts.millisecond.unwrap_or(0.0), 0.0, 999.0, "millisecond")? as u32;
        let local = NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|d| d.and_hms_milli_opt(hour, minute, second, ms))
            .ok_or("invalid calendar datetime")?;
        let zone = Zone::parse(parts.timezone.as_deref())?;
        Self::checked(zone.local(local)?, zone)
    }
    /// 无 format 时解析 RFC3339；无偏移的自定义格式使用显式时区或 UTC。
    fn parse(text: &str, format: Option<String>, timezone: Option<String>) -> Result<Self, String> {
        if text.len() > 4096 || format.as_ref().is_some_and(|f| f.len() > 4096) {
            return Err("datetime input exceeds limit".into());
        }
        if let Some(format) = format {
            let mut parsed = chrono::format::Parsed::new();
            chrono::format::parse(
                &mut parsed,
                text,
                chrono::format::StrftimeItems::new(&format),
            )
            .map_err(|error| format!("invalid datetime: {error}"))?;
            // 纯日期默认午夜；不能用日期解析回退吞掉不完整时间或显式偏移。
            if parsed.hour_div_12().is_none()
                && parsed.hour_mod_12().is_none()
                && parsed.minute().is_none()
                && parsed.second().is_none()
                && parsed.nanosecond().is_none()
                && parsed.timestamp().is_none()
            {
                parsed
                    .set_hour(0)
                    .map_err(|error| format!("invalid datetime: {error}"))?;
                parsed
                    .set_minute(0)
                    .map_err(|error| format!("invalid datetime: {error}"))?;
                parsed
                    .set_second(0)
                    .map_err(|error| format!("invalid datetime: {error}"))?;
            }
            if parsed.offset().is_some() || parsed.timestamp().is_some() {
                let value = parsed
                    .to_datetime()
                    .map_err(|error| format!("invalid datetime: {error}"))?;
                let zone = timezone
                    .as_deref()
                    .map(|z| Zone::parse(Some(z)))
                    .transpose()?
                    .unwrap_or(Zone::Fixed(*value.offset()));
                Self::checked(value.with_timezone(&Utc), zone)
            } else {
                let local = parsed
                    .to_naive_datetime_with_offset(0)
                    .map_err(|error| format!("invalid datetime: {error}"))?;
                let zone = Zone::parse(timezone.as_deref())?;
                Self::checked(zone.local(local)?, zone)
            }
        } else {
            let value = DateTime::parse_from_rfc3339(text)
                .map_err(|error| format!("invalid RFC3339 datetime: {error}"))?;
            let zone = timezone
                .as_deref()
                .map(|z| Zone::parse(Some(z)))
                .transpose()?
                .unwrap_or(Zone::Fixed(*value.offset()));
            Self::checked(value.with_timezone(&Utc), zone)
        }
    }
    fn from_timestamp_ms(timestamp: f64, timezone: Option<String>) -> Result<Self, String> {
        let timestamp = integer(timestamp, -MAX_INTEGER, MAX_INTEGER, "timestamp")?;
        let utc = DateTime::from_timestamp_millis(timestamp)
            .ok_or("datetime timestamp is out of range")?;
        Self::checked(utc, Zone::parse(timezone.as_deref())?)
    }
    /// 纳秒使用十进制文本，避免 Lua number 丢失文件时间戳精度。
    fn from_timestamp_ns(timestamp: &str, timezone: Option<String>) -> Result<Self, String> {
        let digits = timestamp
            .strip_prefix('-')
            .or_else(|| timestamp.strip_prefix('+'))
            .unwrap_or(timestamp);
        if timestamp.len() > 40 || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("nanosecond timestamp must be a decimal integer string".into());
        }
        let value = timestamp
            .parse::<i128>()
            .map_err(|_| "nanosecond timestamp is out of range")?;
        let seconds: i64 = value
            .div_euclid(1_000_000_000)
            .try_into()
            .map_err(|_| "nanosecond timestamp is out of range")?;
        let nanos = value.rem_euclid(1_000_000_000) as u32;
        let utc = DateTime::from_timestamp(seconds, nanos)
            .ok_or("nanosecond timestamp is out of range")?;
        Self::checked(utc, Zone::parse(timezone.as_deref())?)
    }
    fn timestamp_ns(&self) -> String {
        (self.utc.timestamp() as i128 * 1_000_000_000 + self.utc.timestamp_subsec_nanos() as i128)
            .to_string()
    }
    fn timestamp_ms(&self) -> f64 {
        self.utc.timestamp_millis() as f64
    }
    fn timezone(&self) -> String {
        self.zone.name()
    }
    fn with_timezone(&self, timezone: &str) -> Result<Self, String> {
        Self::checked(self.utc, Zone::parse(Some(timezone))?)
    }
    fn to_rfc3339(&self) -> String {
        self.zone
            .at(self.utc)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }
    fn format(&self, pattern: &str) -> Result<String, String> {
        if pattern.len() > 4096 {
            return Err("datetime format exceeds limit".into());
        }
        let mut output = String::new();
        let displayed = self.zone.at(self.utc);
        let result = match self.zone {
            Zone::Fixed(_) => {
                std::fmt::write(&mut output, format_args!("{}", displayed.format(pattern)))
            }
            Zone::Named(zone) => std::fmt::write(
                &mut output,
                format_args!("{}", self.utc.with_timezone(&zone).format(pattern)),
            ),
        };
        result.map_err(|_| "invalid datetime format".to_string())?;
        if output.len() > 65536 {
            return Err("formatted datetime exceeds limit".into());
        }
        Ok(output)
    }
    fn parts(&self) -> Parts {
        let value = self.zone.at(self.utc);
        Parts {
            year: value.year() as i64,
            month: value.month() as i64,
            day: value.day() as i64,
            hour: value.hour() as i64,
            minute: value.minute() as i64,
            second: value.second() as i64,
            millisecond: value.timestamp_subsec_millis() as i64,
            nanosecond: value.timestamp_subsec_nanos() as i64,
            weekday: value.weekday().number_from_monday() as i64,
            ordinal: value.ordinal() as i64,
            iso_year: value.iso_week().year() as i64,
            iso_week: value.iso_week().week() as i64,
            days_in_month: month_days(value.year(), value.month()) as i64,
            offset_seconds: value.offset().fix().local_minus_utc() as i64,
            timezone: self.zone.name(),
        }
    }
    /// 经过的毫秒数；不是按本地日历加一天，DST 边界下两者不同。
    fn add_ms(&self, millis: f64) -> Result<Self, String> {
        let millis = integer(millis, -MAX_INTEGER, MAX_INTEGER, "duration")?;
        let utc = self
            .utc
            .checked_add_signed(chrono::TimeDelta::milliseconds(millis))
            .ok_or("datetime is out of range")?;
        Self::checked(utc, self.zone)
    }
    /// 先合并年月，再按当地日历加周/日，保留本地时分秒与纳秒。
    fn add_calendar(&self, delta: Calendar) -> Result<Self, String> {
        let years = integer(delta.years, -9999.0, 9999.0, "calendar years")?;
        let months = integer(delta.months, -119988.0, 119988.0, "calendar months")?;
        let weeks = integer(delta.weeks, -521722.0, 521722.0, "calendar weeks")?;
        let days = integer(delta.days, -3652058.0, 3652058.0, "calendar days")?;
        let overflow = delta.overflow.as_deref().unwrap_or("clamp");
        if !matches!(overflow, "clamp" | "reject") {
            return Err("calendar overflow must be clamp or reject".into());
        }
        let displayed = self.zone.at(self.utc);
        let original = displayed.date_naive();
        let total = original.year() as i64 * 12 + original.month0() as i64 + years * 12 + months;
        let year = total.div_euclid(12);
        let month = (total.rem_euclid(12) + 1) as u32;
        if !(1..=9999).contains(&year) {
            return Err("calendar year must be in 1..=9999".into());
        }
        let last = month_days(year as i32, month);
        let day = if overflow == "clamp" {
            original.day().min(last)
        } else {
            original.day()
        };
        let date = NaiveDate::from_ymd_opt(year as i32, month, day)
            .ok_or("calendar month has no requested day")?
            .checked_add_signed(chrono::TimeDelta::days(days + weeks * 7))
            .ok_or("calendar date is out of range")?;
        let local = date.and_time(displayed.time());
        let ambiguity = delta.ambiguity.as_deref().unwrap_or("reject");
        if local == displayed.naive_local() && ambiguity == "reject" {
            return Ok(self.clone());
        }
        Self::checked(
            self.zone
                .resolve(local, delta.ambiguity.as_deref().unwrap_or("reject"))?,
            self.zone,
        )
    }
    fn start_of(&self, unit: &str, ambiguity: Option<String>) -> Result<Self, String> {
        let original = self.zone.at(self.utc).date_naive();
        let date = match unit {
            "day" => Some(original),
            "week" => original.checked_sub_signed(chrono::TimeDelta::days(
                original.weekday().num_days_from_monday() as i64,
            )),
            "month" => original.with_day(1),
            "year" => NaiveDate::from_ymd_opt(original.year(), 1, 1),
            _ => return Err("datetime period must be day, week, month or year".into()),
        }
        .ok_or("datetime period is out of range")?;
        Self::checked(
            self.zone.resolve(
                date.and_hms_opt(0, 0, 0).unwrap(),
                ambiguity.as_deref().unwrap_or("reject"),
            )?,
            self.zone,
        )
    }
    fn parse_rfc2822(text: &str, timezone: Option<String>) -> Result<Self, String> {
        if text.len() > 4096 {
            return Err("datetime input exceeds limit".into());
        }
        let value = DateTime::parse_from_rfc2822(text)
            .map_err(|err| format!("invalid RFC2822 datetime: {err}"))?;
        let zone = timezone
            .as_deref()
            .map(|name| Zone::parse(Some(name)))
            .transpose()?
            .unwrap_or(Zone::Fixed(*value.offset()));
        Self::checked(value.with_timezone(&Utc), zone)
    }
    fn to_rfc2822(&self) -> String {
        self.zone.at(self.utc).to_rfc2822()
    }
    fn diff_ms(&self, earlier: Datetime) -> f64 {
        (self.utc - earlier.utc).num_milliseconds() as f64
    }
}
