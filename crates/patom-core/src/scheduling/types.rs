//! Domain types for the scheduling subsystem.
//!
//! CLAUDE.md §1: every value carrying an invariant gets a newtype with a
//! `TryFrom` smart constructor. The HTTP/tool boundary parses raw JSON
//! into these types once; nothing downstream constructs them directly.

use std::fmt;
use std::sync::Arc;

use chrono::offset::MappedLocalTime;
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::runtime::PromptRequestId;
use crate::types::{PROMPT_MAX_BYTES, ParseError};

use super::limits::SCHEDULED_TASK_NAME_MAX_LEN;

crate::uuid_newtype! {
    /// Opaque identifier for a `scheduled_tasks` row. Wire format and DB
    /// column both use `task_id`; this is the typed handle.
    pub ScheduledTaskId
}

// --- ScheduledTaskName ----------------------------------------------------

/// Operator-/agent-chosen display name. Used for tool listings and tracing
/// only — never read by the model as part of a fired prompt.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ScheduledTaskName(Arc<str>);

impl ScheduledTaskName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ScheduledTaskName {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "scheduled_task_name",
            });
        }
        if raw.len() > SCHEDULED_TASK_NAME_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "scheduled_task_name",
                max: SCHEDULED_TASK_NAME_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for ScheduledTaskName {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for ScheduledTaskName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ScheduledTaskName").field(&&*self.0).finish()
    }
}

impl fmt::Display for ScheduledTaskName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ScheduledTaskName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ScheduledTaskName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

// --- ScheduledPrompt ------------------------------------------------------

/// The prompt body fired into the agent on each scheduled wake-up. Same
/// byte cap as the HTTP `Prompt` boundary so a fired row is always
/// acceptable input to the queue.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ScheduledPrompt(String);

impl ScheduledPrompt {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for ScheduledPrompt {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        // Mirror `Prompt::try_from` — whitespace-only must not pass the
        // tool boundary, because the scheduler re-parses through
        // `Prompt` at fire time and would otherwise produce a poisoned
        // row.
        if raw.trim().is_empty() {
            return Err(ParseError::Empty {
                field: "scheduled_prompt",
            });
        }
        if raw.len() > PROMPT_MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "scheduled_prompt",
                max: PROMPT_MAX_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(raw))
    }
}

impl TryFrom<&str> for ScheduledPrompt {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::try_from(raw.to_string())
    }
}

impl fmt::Debug for ScheduledPrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Same shape as Prompt — never include the body in default debug,
        // it can be sensitive (mail summary instructions, etc).
        f.debug_struct("ScheduledPrompt")
            .field("len", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl Serialize for ScheduledPrompt {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ScheduledPrompt {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

// --- Weekday + Weekdays bitset --------------------------------------------

crate::str_enum! {
    /// Day-of-week. Independent of `chrono::Weekday` so the wire format is
    /// stably lowercase and so the bit ordering used by [`Weekdays`] is
    /// pinned by the variant order rather than chrono's API.
    pub enum Weekday {
        Mon => "mon",
        Tue => "tue",
        Wed => "wed",
        Thu => "thu",
        Fri => "fri",
        Sat => "sat",
        Sun => "sun",
    }
}

impl Weekday {
    /// Bit position in the [`Weekdays`] bitset.
    #[must_use]
    pub const fn bit(self) -> u8 {
        match self {
            Self::Mon => 0,
            Self::Tue => 1,
            Self::Wed => 2,
            Self::Thu => 3,
            Self::Fri => 4,
            Self::Sat => 5,
            Self::Sun => 6,
        }
    }

    /// Convert from `chrono::Weekday`. Used by `next_after` once a
    /// candidate calendar date has been computed.
    #[must_use]
    pub const fn from_chrono(c: chrono::Weekday) -> Self {
        match c {
            chrono::Weekday::Mon => Self::Mon,
            chrono::Weekday::Tue => Self::Tue,
            chrono::Weekday::Wed => Self::Wed,
            chrono::Weekday::Thu => Self::Thu,
            chrono::Weekday::Fri => Self::Fri,
            chrono::Weekday::Sat => Self::Sat,
            chrono::Weekday::Sun => Self::Sun,
        }
    }
}

/// Bitset of [`Weekday`] values.
///
/// Wire form is a JSON array of weekday strings; in memory it's a single
/// byte. Empty sets are rejected at parse time — a recurring schedule
/// with no fire days would never fire.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Weekdays(u8);

impl Weekdays {
    /// Every day of the week.
    pub const ALL: Self = Self(0b0111_1111);
    /// Mon..Fri inclusive.
    pub const WORKDAYS: Self = Self(0b0001_1111);
    /// Sat + Sun.
    pub const WEEKENDS: Self = Self(0b0110_0000);

    /// Smart constructor used by `Deserialize`. Empty input is rejected.
    pub fn try_from_iter<I: IntoIterator<Item = Weekday>>(iter: I) -> Result<Self, ParseError> {
        let mut bits = 0u8;
        for w in iter {
            bits |= 1 << w.bit();
        }
        if bits == 0 {
            return Err(ParseError::Empty { field: "weekdays" });
        }
        Ok(Self(bits))
    }

    /// Raw bits — for tests and debugging only; encoded form is the
    /// `Serialize` impl.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns whether `w` is in the set.
    #[must_use]
    pub const fn contains(self, w: Weekday) -> bool {
        (self.0 & (1 << w.bit())) != 0
    }

    /// Iterate set members in Mon..Sun order. Bounded by 7 — caller
    /// can trust no allocation.
    pub fn iter(self) -> impl Iterator<Item = Weekday> {
        const ALL_DAYS: [Weekday; 7] = [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
            Weekday::Sat,
            Weekday::Sun,
        ];
        ALL_DAYS.into_iter().filter(move |w| self.contains(*w))
    }
}

impl fmt::Debug for Weekdays {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl Serialize for Weekdays {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let days: Vec<Weekday> = self.iter().collect();
        days.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Weekdays {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v: Vec<Weekday> = Vec::deserialize(d)?;
        Self::try_from_iter(v).map_err(serde::de::Error::custom)
    }
}

// --- TimeOfDay ------------------------------------------------------------

/// Wall-clock time of day, interpreted in a [`Timezone`] by `next_after`.
/// Wire form: `"HH:MM"` zero-padded.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeOfDay {
    hour: u8,
    minute: u8,
}

impl TimeOfDay {
    pub fn try_new(hour: u8, minute: u8) -> Result<Self, ParseError> {
        if hour > 23 {
            return Err(ParseError::OutOfRange {
                field: "time_of_day.hour",
                detail: "0..=23",
            });
        }
        if minute > 59 {
            return Err(ParseError::OutOfRange {
                field: "time_of_day.minute",
                detail: "0..=59",
            });
        }
        Ok(Self { hour, minute })
    }

    #[must_use]
    pub const fn hour(self) -> u8 {
        self.hour
    }

    #[must_use]
    pub const fn minute(self) -> u8 {
        self.minute
    }
}

impl fmt::Debug for TimeOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

impl fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

impl Serialize for TimeOfDay {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&format_args!("{:02}:{:02}", self.hour, self.minute))
    }
}

impl<'de> Deserialize<'de> for TimeOfDay {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        // Format: HH:MM zero-padded.
        let bytes = raw.as_bytes();
        if bytes.len() != 5 || bytes[2] != b':' {
            return Err(serde::de::Error::custom(format!(
                "time_of_day must be HH:MM, got {raw:?}"
            )));
        }
        let hour: u8 = raw[0..2]
            .parse()
            .map_err(|_| serde::de::Error::custom(format!("time_of_day hour: {raw:?}")))?;
        let minute: u8 = raw[3..5]
            .parse()
            .map_err(|_| serde::de::Error::custom(format!("time_of_day minute: {raw:?}")))?;
        Self::try_new(hour, minute).map_err(serde::de::Error::custom)
    }
}

// --- Timezone -------------------------------------------------------------

/// IANA-named timezone wrapping `chrono_tz::Tz`. Newtyped so the rest of
/// the codebase never sees a raw string and the smart constructor is the
/// only entry point (CLAUDE.md §1).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timezone(Tz);

impl Timezone {
    #[must_use]
    pub const fn from_tz(tz: Tz) -> Self {
        Self(tz)
    }

    #[must_use]
    pub const fn into_tz(self) -> Tz {
        self.0
    }

    /// IANA name (`"Asia/Bangkok"`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.0.name()
    }
}

impl TryFrom<&str> for Timezone {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        raw.parse::<Tz>()
            .map(Self)
            .map_err(|_| ParseError::Malformed {
                field: "timezone",
                detail: "not a valid IANA name",
            })
    }
}

impl TryFrom<String> for Timezone {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for Timezone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Timezone").field(&self.0.name()).finish()
    }
}

impl fmt::Display for Timezone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.name())
    }
}

impl Serialize for Timezone {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0.name())
    }
}

impl<'de> Deserialize<'de> for Timezone {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

// --- DefaultTimezone resolver --------------------------------------------

/// Resolves the timezone applied to `Recurring` schedules.
///
/// Used when the tool call omits `tz`. Today this is process-wide; the
/// future per-organisation override would load by `agent_id` and fall
/// back to the wrapped value.
#[derive(Clone, Debug)]
pub struct DefaultTimezone(Timezone);

impl DefaultTimezone {
    #[must_use]
    pub const fn from_timezone(tz: Timezone) -> Self {
        Self(tz)
    }

    /// Resolve the default timezone for `_agent`. Today: returns the
    /// process-wide value. Future: looks up the agent's organisation and
    /// returns the org default, with this value as the fallback.
    #[must_use]
    pub const fn for_agent(&self, _agent: AgentId) -> Timezone {
        self.0
    }
}

// --- ScheduleSpec ---------------------------------------------------------

/// Forward-looking description of when a task fires.
///
/// Two variants cover the personal-assistant scheduling surface; calendar
/// patterns (nth-weekday-of-month etc.) are deliberately out of scope —
/// agents that need them self-reschedule a `Once`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ScheduleSpec {
    /// Single fire at a specific instant.
    Once { run_at: DateTime<Utc> },
    /// Fires on each set weekday at `time` in `tz`.
    Recurring {
        weekdays: Weekdays,
        time: TimeOfDay,
        tz: Timezone,
    },
}

impl ScheduleSpec {
    /// Smallest fire instant strictly greater than `now`, or `None` when
    /// the schedule has no further fires (one-shot in the past, or an
    /// empty weekday set — the latter cannot occur via the `Deserialize`
    /// smart constructor but the function handles it positively per
    /// CLAUDE.md §4).
    #[must_use]
    pub fn next_after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::Once { run_at } => {
                if *run_at > now {
                    Some(*run_at)
                } else {
                    None
                }
            }
            Self::Recurring { weekdays, time, tz } => {
                if weekdays.is_empty() {
                    return None;
                }
                next_recurring(now, *weekdays, *time, *tz)
            }
        }
    }
}

/// Walk up to 8 candidate days forward (1 extra past the 7-day weekday
/// cycle to absorb a single DST skip-day) and pick the first matching
/// fire instant strictly greater than `now`.
fn next_recurring(
    now: DateTime<Utc>,
    weekdays: Weekdays,
    time: TimeOfDay,
    tz: Timezone,
) -> Option<DateTime<Utc>> {
    let tz = tz.into_tz();
    let now_local = now.with_timezone(&tz);
    let today = now_local.date_naive();
    let mut offset = 0i64;
    while offset <= 8 {
        let date: NaiveDate = today + Duration::days(offset);
        offset += 1;
        let local_w = Weekday::from_chrono(date.weekday());
        if !weekdays.contains(local_w) {
            continue;
        }
        let mapped = tz.with_ymd_and_hms(
            date.year(),
            date.month(),
            date.day(),
            u32::from(time.hour()),
            u32::from(time.minute()),
            0,
        );
        let local_dt = match mapped {
            // Spring-forward: chosen wall-time doesn't exist — skip to next match.
            MappedLocalTime::None => continue,
            MappedLocalTime::Single(dt) => dt,
            // Fall-back: wall-time exists twice; pick the earliest occurrence
            // (per RFC 5545 default behavior).
            MappedLocalTime::Ambiguous(earliest, _later) => earliest,
        };
        let utc_dt = local_dt.with_timezone(&Utc);
        if utc_dt > now {
            return Some(utc_dt);
        }
    }
    None
}

// --- ScheduledTaskState + ScheduledTaskRecord ----------------------------

crate::str_enum! {
    /// Lifecycle state of a `scheduled_tasks` row. The `state` `CHECK`
    /// constraint in migration 11 is keyed off these labels exactly.
    pub enum ScheduledTaskState {
        Active    => "active",
        Done      => "done",
        Cancelled => "cancelled",
    }
}

/// Materialised view of one `scheduled_tasks` row.
///
/// `org_id` + `created_by_user_id` are the tenancy projection added by
/// migration 19. The scheduler reads them off the row at fire-time and
/// pins the enqueued `prompt_requests` row to the same tenant; no
/// downstream JOIN through `agents`/`org_members` is needed.
#[derive(Clone, Debug)]
pub struct ScheduledTaskRecord {
    pub id: ScheduledTaskId,
    pub owner_agent_id: AgentId,
    pub org_id: OrgId,
    pub created_by_user_id: UserId,
    pub name: ScheduledTaskName,
    pub prompt: ScheduledPrompt,
    pub schedule: ScheduleSpec,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub last_request_id: Option<PromptRequestId>,
    pub state: ScheduledTaskState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono_tz::America::New_York;
    use chrono_tz::Asia::Bangkok;

    #[test]
    fn weekday_str_round_trip() {
        for w in Weekday::ALL.iter().copied() {
            assert_eq!(Weekday::parse(w.as_str()), Some(w));
        }
    }

    #[test]
    fn weekdays_workdays_and_weekends_match_iter() {
        let workdays: Vec<Weekday> = Weekdays::WORKDAYS.iter().collect();
        assert_eq!(
            workdays,
            vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri
            ]
        );
        let weekends: Vec<Weekday> = Weekdays::WEEKENDS.iter().collect();
        assert_eq!(weekends, vec![Weekday::Sat, Weekday::Sun]);
        assert_eq!(Weekdays::ALL.iter().count(), 7);
    }

    #[test]
    fn weekdays_serde_round_trip() {
        let json = serde_json::json!(["mon", "wed", "fri"]);
        let parsed: Weekdays = serde_json::from_value(json).expect("ok");
        assert!(parsed.contains(Weekday::Mon));
        assert!(parsed.contains(Weekday::Wed));
        assert!(parsed.contains(Weekday::Fri));
        assert!(!parsed.contains(Weekday::Sat));

        let back = serde_json::to_value(parsed).expect("ok");
        assert_eq!(back, serde_json::json!(["mon", "wed", "fri"]));
    }

    #[test]
    fn weekdays_empty_array_is_rejected() {
        let json = serde_json::json!([]);
        assert!(serde_json::from_value::<Weekdays>(json).is_err());
    }

    #[test]
    fn time_of_day_round_trip() {
        let t = TimeOfDay::try_new(5, 30).expect("ok");
        let v = serde_json::to_value(t).expect("ok");
        assert_eq!(v, serde_json::json!("05:30"));
        let back: TimeOfDay = serde_json::from_value(v).expect("ok");
        assert_eq!(back, t);
    }

    #[test]
    fn time_of_day_rejects_out_of_range() {
        assert!(TimeOfDay::try_new(24, 0).is_err());
        assert!(TimeOfDay::try_new(0, 60).is_err());
        assert!(serde_json::from_value::<TimeOfDay>(serde_json::json!("25:00")).is_err());
        assert!(serde_json::from_value::<TimeOfDay>(serde_json::json!("5:00")).is_err());
        assert!(serde_json::from_value::<TimeOfDay>(serde_json::json!("05-00")).is_err());
    }

    #[test]
    fn timezone_round_trip() {
        let tz: Timezone = serde_json::from_value(serde_json::json!("Asia/Bangkok")).expect("ok");
        assert_eq!(tz.name(), "Asia/Bangkok");
        let back = serde_json::to_value(tz).expect("ok");
        assert_eq!(back, serde_json::json!("Asia/Bangkok"));
    }

    #[test]
    fn timezone_rejects_unknown() {
        assert!(serde_json::from_value::<Timezone>(serde_json::json!("Mars/Olympus")).is_err());
    }

    #[test]
    fn schedule_once_round_trip() {
        let json = serde_json::json!({
            "kind": "once",
            "data": { "run_at": "2026-12-31T23:00:00Z" }
        });
        let s: ScheduleSpec = serde_json::from_value(json.clone()).expect("ok");
        let back = serde_json::to_value(&s).expect("ok");
        assert_eq!(back, json);
    }

    #[test]
    fn schedule_recurring_round_trip() {
        let json = serde_json::json!({
            "kind": "recurring",
            "data": {
                "weekdays": ["mon","tue","wed","thu","fri"],
                "time": "05:00",
                "tz": "Asia/Bangkok"
            }
        });
        let s: ScheduleSpec = serde_json::from_value(json.clone()).expect("ok");
        let back = serde_json::to_value(&s).expect("ok");
        assert_eq!(back, json);
    }

    fn utc(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, hh, mm, 0)
            .single()
            .expect("test fixture: utc datetime is unambiguous")
    }

    #[test]
    fn next_after_once_future_returns_value() {
        let s = ScheduleSpec::Once {
            run_at: utc(2030, 1, 1, 0, 0),
        };
        let now = utc(2026, 5, 14, 12, 0);
        assert_eq!(s.next_after(now), Some(utc(2030, 1, 1, 0, 0)));
    }

    #[test]
    fn next_after_once_past_returns_none() {
        let s = ScheduleSpec::Once {
            run_at: utc(2020, 1, 1, 0, 0),
        };
        let now = utc(2026, 5, 14, 12, 0);
        assert_eq!(s.next_after(now), None);
    }

    #[test]
    fn next_after_once_equal_returns_none() {
        // Strictly forward — equal is not future.
        let when = utc(2026, 5, 14, 12, 0);
        let s = ScheduleSpec::Once { run_at: when };
        assert_eq!(s.next_after(when), None);
    }

    #[test]
    fn recurring_daily_in_bangkok_picks_today_when_before_time() {
        // Now is 2026-05-14 22:00 UTC = 2026-05-15 05:00 Bangkok.
        // Wait — we want now BEFORE today's fire, so use 21:00 UTC = 04:00 BKK.
        // Actually we want now BEFORE 05:00 BKK so today's 05:00 BKK fires.
        // 2026-05-14 21:00 UTC = 2026-05-15 04:00 BKK -> next fire = 2026-05-15
        // 05:00 BKK = 2026-05-14 22:00 UTC.
        let s = ScheduleSpec::Recurring {
            weekdays: Weekdays::ALL,
            time: TimeOfDay::try_new(5, 0).expect("valid HH:MM"),
            tz: Timezone::from_tz(Bangkok),
        };
        let now = utc(2026, 5, 14, 21, 0);
        let fired = s.next_after(now).expect("future");
        assert_eq!(fired, utc(2026, 5, 14, 22, 0));
    }

    #[test]
    fn recurring_daily_in_bangkok_skips_to_tomorrow_when_after_time() {
        // Now is 2026-05-14 23:00 UTC = 2026-05-15 06:00 BKK -> after today's
        // 05:00 BKK fire, so the next fire is 2026-05-16 05:00 BKK = 2026-05-15
        // 22:00 UTC.
        let s = ScheduleSpec::Recurring {
            weekdays: Weekdays::ALL,
            time: TimeOfDay::try_new(5, 0).expect("valid HH:MM"),
            tz: Timezone::from_tz(Bangkok),
        };
        let now = utc(2026, 5, 14, 23, 0);
        let fired = s.next_after(now).expect("future");
        assert_eq!(fired, utc(2026, 5, 15, 22, 0));
    }

    #[test]
    fn recurring_workdays_skips_weekend() {
        // 2026-05-15 is a Friday; the next workday after Friday is Monday.
        // Friday 22:00 BKK = Friday 15:00 UTC. So a "now" of Friday 16:00
        // UTC (after the fire) should skip Sat+Sun and land on Monday.
        let s = ScheduleSpec::Recurring {
            weekdays: Weekdays::WORKDAYS,
            time: TimeOfDay::try_new(22, 0).expect("valid HH:MM"),
            tz: Timezone::from_tz(Bangkok),
        };
        // Friday 2026-05-15, after today's fire.
        let now = utc(2026, 5, 15, 16, 0);
        let fired = s.next_after(now).expect("future");
        // Monday 2026-05-18 22:00 BKK = 2026-05-18 15:00 UTC.
        assert_eq!(fired, utc(2026, 5, 18, 15, 0));
    }

    #[test]
    fn recurring_dst_spring_forward_skip_day() {
        // America/New_York spring-forward 2026-03-08: 02:00 -> 03:00 local.
        // A 02:30 schedule on that day doesn't exist; next_after should
        // skip to 2026-03-09 02:30.
        let s = ScheduleSpec::Recurring {
            weekdays: Weekdays::ALL,
            time: TimeOfDay::try_new(2, 30).expect("valid HH:MM"),
            tz: Timezone::from_tz(New_York),
        };
        // Just before 02:00 local on 2026-03-08 = 06:30 UTC.
        let now = utc(2026, 3, 8, 6, 30);
        let fired = s.next_after(now).expect("future");
        // 2026-03-09 02:30 EDT = 2026-03-09 06:30 UTC.
        assert_eq!(fired, utc(2026, 3, 9, 6, 30));
    }

    #[test]
    fn recurring_dst_fall_back_picks_earliest() {
        // America/New_York fall-back 2026-11-01: 02:00 -> 01:00 local, so
        // 01:30 occurs twice. We pick the earlier occurrence.
        let s = ScheduleSpec::Recurring {
            weekdays: Weekdays::ALL,
            time: TimeOfDay::try_new(1, 30).expect("valid HH:MM"),
            tz: Timezone::from_tz(New_York),
        };
        // Now is 2026-10-31 23:00 UTC — well before the fall-back.
        let now = utc(2026, 10, 31, 23, 0);
        let fired = s.next_after(now).expect("future");
        // The earlier (EDT) occurrence: 01:30 EDT = 05:30 UTC.
        assert_eq!(fired, utc(2026, 11, 1, 5, 30));
    }
}
