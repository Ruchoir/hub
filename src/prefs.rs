//! Notification preferences, held by the hub so that it can obey them (design `docs/design/hub.md`,
//! section 4; decision 9.2).
//!
//! From the monolith's preferences, reshaped around the inbox's categories. The preferences are
//! shared: a product applies what only it knows (a conversation's level and mute, who hears every
//! message) before it publishes, so its recipients are already filtered. The hub applies the rest:
//!
//! - **the person's own settings** ([`NotificationPrefs`]): a master switch, which categories reach
//!   them in the app (the inbox and push) and which by mail, quiet hours;
//! - **a level per organisation** (`all`, `mentions` or `none`) for what happens in it;
//! - **do not disturb**, the presence they chose.
//!
//! A level decides how much an organisation says: `mentions` keeps what names or addresses the person
//! (`mention`, `direct`), `none` silences its conversations. A reminder, an invitation or a notice is
//! asked for or addressed to the person and passes any level.
//!
//! Quiet hours and do not disturb decide *when*, not *whether*: what cannot interrupt now is in the
//! inbox, and the mail waits for the window to close. Quiet hours are kept as the local wall-clock
//! times the person typed, with the offset of their clock from UTC that the client refreshes on every
//! load: no time-zone database on the server.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use ruchoir_http::Problem;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::Person;
use crate::AppState;

/// The largest offset from UTC a clock can have (UTC+14, Kiribati), in minutes.
const MAX_OFFSET_MINUTES: i32 = 14 * 60;

/// Which categories reach someone through one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Categories {
    pub mention: bool,
    pub reply: bool,
    pub direct: bool,
    pub activity: bool,
    pub reminder: bool,
    pub invitation: bool,
    pub notice: bool,
}

impl Categories {
    fn wants(&self, category: &str) -> bool {
        match category {
            "mention" => self.mention,
            "reply" => self.reply,
            "direct" => self.direct,
            "activity" => self.activity,
            "reminder" => self.reminder,
            "invitation" => self.invitation,
            "notice" => self.notice,
            _ => false,
        }
    }
}

impl Default for Categories {
    fn default() -> Self {
        Self {
            mention: true,
            reply: true,
            direct: true,
            activity: true,
            reminder: true,
            invitation: true,
            notice: true,
        }
    }
}

/// A person's own notification settings.
///
/// Every field has a default, and a stored document missing a field (written before it existed)
/// takes that default rather than failing to load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct NotificationPrefs {
    /// Master switch: off, nothing notifies, anywhere.
    pub enabled: bool,
    /// Play a sound with a notification.
    pub sound: bool,
    /// What reaches them in the app and by push.
    pub app: Categories,
    /// The mail fallback's master switch, then what reaches them by mail.
    pub email: bool,
    pub email_categories: Categories,
    /// Hold notifications back during the window below.
    pub quiet_hours: bool,
    /// Start of the quiet window, `HH:MM`, local time; may be later than `quiet_to` (overnight).
    pub quiet_from: String,
    /// End of the quiet window, `HH:MM`, local time.
    pub quiet_to: String,
    /// Offset of the person's clock from UTC, in minutes (`+120` in Paris in summer).
    pub utc_offset_minutes: i32,
}

impl Default for NotificationPrefs {
    fn default() -> Self {
        Self {
            enabled: true,
            // Théo, 2026-10-21: notifications ring by default; the person turns the sound off.
            sound: true,
            app: Categories::default(),
            email: true,
            // A reaction or an edit is never worth a message in someone's mailbox.
            email_categories: Categories {
                activity: false,
                ..Categories::default()
            },
            quiet_hours: false,
            quiet_from: "21:00".to_owned(),
            quiet_to: "08:00".to_owned(),
            utc_offset_minutes: 0,
        }
    }
}

/// Where a notification would go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The inbox, the open pages and push.
    App,
    Email,
}

/// Minutes since midnight for an `HH:MM` string, or `None` when it is not one.
fn minutes_of_day(hhmm: &str) -> Option<i32> {
    let (h, m) = hhmm.split_once(':')?;
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some(h * 60 + m)
}

impl NotificationPrefs {
    /// Refuse a document the hub could not act on, rather than storing it and guessing later.
    fn validate(&self) -> Result<(), &'static str> {
        if minutes_of_day(&self.quiet_from).is_none() || minutes_of_day(&self.quiet_to).is_none() {
            return Err("quiet_hours");
        }
        if self.utc_offset_minutes.abs() > MAX_OFFSET_MINUTES {
            return Err("utc_offset");
        }
        Ok(())
    }

    /// Whether `at` falls inside the quiet window, read in the person's local time.
    ///
    /// The window may run past midnight (21:00 to 08:00 is the ordinary case), so it is read as two
    /// ranges when it starts later than it ends. An empty window (start equals end) is never quiet.
    pub fn in_quiet_hours(&self, at: OffsetDateTime) -> bool {
        if !self.quiet_hours {
            return false;
        }
        let (Some(from), Some(to)) = (
            minutes_of_day(&self.quiet_from),
            minutes_of_day(&self.quiet_to),
        ) else {
            return false;
        };
        if from == to {
            return false;
        }
        let utc = i32::from(at.hour()) * 60 + i32::from(at.minute());
        let now = (utc + self.utc_offset_minutes).rem_euclid(24 * 60);
        if from < to {
            now >= from && now < to
        } else {
            now >= from || now < to
        }
    }
}

/// The levels an organisation may be set to.
pub const LEVELS: [&str; 3] = ["all", "mentions", "none"];

/// Whether a notification of `category` from an organisation at `level` (none set: `all`) reaches
/// someone by `delivery`. Quiet hours and do not disturb are not part of this: they decide when.
pub fn allows(
    category: &str,
    prefs: &NotificationPrefs,
    level: Option<&str>,
    delivery: Delivery,
) -> bool {
    if !prefs.enabled {
        return false;
    }
    let wanted = match delivery {
        Delivery::App => prefs.app.wants(category),
        Delivery::Email => prefs.email && prefs.email_categories.wants(category),
    };
    if !wanted {
        return false;
    }
    // Asked for or addressed to the person: no organisation's level holds it back.
    if matches!(category, "reminder" | "invitation" | "notice") {
        return true;
    }
    match level.unwrap_or("all") {
        "none" => false,
        "mentions" => matches!(category, "mention" | "direct"),
        _ => true,
    }
}

/// Whether someone may be interrupted right now: outside their quiet hours and not in "do not
/// disturb". What cannot interrupt them now is not lost: the inbox has it, and the mail waits.
pub fn may_interrupt(
    prefs: &NotificationPrefs,
    chosen_presence: Option<&str>,
    at: OffsetDateTime,
) -> bool {
    chosen_presence != Some("dnd") && !prefs.in_quiet_hours(at)
}

/// Read someone's settings, with the defaults for anything never saved.
pub async fn load(db: &impl ConnectionTrait, account: Uuid) -> Result<NotificationPrefs, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT prefs FROM notification_settings WHERE account_id = $1",
            [account.into()],
        ))
        .await?;
    Ok(match row {
        Some(row) => serde_json::from_value(row.try_get("", "prefs")?).unwrap_or_default(),
        None => NotificationPrefs::default(),
    })
}

/// Someone's level for an organisation, when they set one.
pub async fn level(
    db: &impl ConnectionTrait,
    account: Uuid,
    organisation: Uuid,
) -> Result<Option<String>, DbErr> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT level FROM organisation_levels WHERE account_id = $1 AND organisation_id = $2",
            [account.into(), organisation.into()],
        ))
        .await?;
    row.map(|r| r.try_get("", "level")).transpose()
}

/// The recipients of a publication who want it by `delivery`, in their order.
pub async fn wanting(
    db: &impl ConnectionTrait,
    recipients: &[Uuid],
    category: &str,
    organisation: Option<Uuid>,
    delivery: Delivery,
) -> Result<Vec<Uuid>, DbErr> {
    let mut kept = Vec::with_capacity(recipients.len());
    for account in recipients {
        let prefs = load(db, *account).await?;
        let level = match organisation {
            Some(organisation) => level(db, *account, organisation).await?,
            None => None,
        };
        if allows(category, &prefs, level.as_deref(), delivery) {
            kept.push(*account);
        }
    }
    Ok(kept)
}

/// Read one's own notification settings.
#[utoipa::path(
    get,
    path = "/api/v1/me/notification-preferences",
    operation_id = "read_notification_preferences",
    tag = "preferences",
    responses((status = 200, description = "The settings, with the defaults for what was never saved", body = NotificationPrefs))
)]
pub async fn read(
    State(state): State<AppState>,
    person: Person,
) -> Result<Json<NotificationPrefs>, Problem> {
    Ok(Json(
        load(&state.db, person.account)
            .await
            .map_err(|_| Problem::internal())?,
    ))
}

/// Save one's own notification settings, whole.
#[utoipa::path(
    put,
    path = "/api/v1/me/notification-preferences",
    tag = "preferences",
    request_body = NotificationPrefs,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not settings the hub can act on (`reason`: `quiet_hours`, `utc_offset`)")
    )
)]
pub async fn save(
    State(state): State<AppState>,
    person: Person,
    Json(prefs): Json<NotificationPrefs>,
) -> Result<StatusCode, Problem> {
    prefs
        .validate()
        .map_err(|why| Problem::bad_request().with_reason(why))?;
    let document = serde_json::to_value(&prefs).map_err(|_| Problem::internal())?;
    state
        .db
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO notification_settings (account_id, prefs) VALUES ($1, $2) \
             ON CONFLICT (account_id) DO UPDATE SET prefs = $2, updated_at = now()",
            [person.account.into(), document.into()],
        ))
        .await
        .map_err(|_| Problem::internal())?;
    Ok(StatusCode::NO_CONTENT)
}

/// A level for one organisation.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Level {
    /// `all`, `mentions`, `none`, or `null` to go back to `all`.
    pub level: Option<String>,
}

/// Set how much one organisation notifies the person.
#[utoipa::path(
    put,
    path = "/api/v1/me/organisations/{id}/notification-level",
    tag = "preferences",
    params(("id" = Uuid, Path, description = "The organisation")),
    request_body = Level,
    responses(
        (status = 204, description = "Set"),
        (status = 400, description = "Not a level"),
        (status = 404, description = "Not one of the person's organisations")
    )
)]
pub async fn set_level(
    State(state): State<AppState>,
    person: Person,
    Path(id): Path<Uuid>,
    Json(body): Json<Level>,
) -> Result<StatusCode, Problem> {
    let internal = |_| Problem::internal();
    if !crate::replica::is_member(&state.db, id, person.account)
        .await
        .map_err(internal)?
    {
        return Err(Problem::not_found());
    }
    let statement = match body.level.as_deref() {
        None => Statement::from_sql_and_values(
            DbBackend::Postgres,
            "DELETE FROM organisation_levels WHERE account_id = $1 AND organisation_id = $2",
            [person.account.into(), id.into()],
        ),
        Some(level) if LEVELS.contains(&level) => Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO organisation_levels (account_id, organisation_id, level) VALUES ($1, $2, $3) \
             ON CONFLICT (account_id, organisation_id) DO UPDATE SET level = $3",
            [person.account.into(), id.into(), level.into()],
        ),
        Some(_) => return Err(Problem::bad_request().with_reason("level")),
    };
    state.db.execute_raw(statement).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_account_hears_its_notifications() {
        // Théo, 2026-10-21: notifications ring by default; the person turns the sound off.
        assert!(NotificationPrefs::default().sound);
    }

    /// A moment at `h:m` UTC. The date is irrelevant to a daily window.
    fn utc(h: i64, m: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(h * 3600 + m * 60).unwrap()
    }

    fn quiet(from: &str, to: &str, offset: i32) -> NotificationPrefs {
        NotificationPrefs {
            quiet_hours: true,
            quiet_from: from.to_owned(),
            quiet_to: to.to_owned(),
            utc_offset_minutes: offset,
            ..NotificationPrefs::default()
        }
    }

    #[test]
    fn an_overnight_window_is_read_in_local_time() {
        // 21:00 to 08:00 in Paris in summer (UTC+2). 20:30 UTC is 22:30 there: quiet.
        let prefs = quiet("21:00", "08:00", 120);
        assert!(prefs.in_quiet_hours(utc(20, 30)));
        // 06:30 UTC is 08:30 there: awake again.
        assert!(!prefs.in_quiet_hours(utc(6, 30)));
        // 04:00 UTC is 06:00 there: still the night.
        assert!(prefs.in_quiet_hours(utc(4, 0)));
    }

    #[test]
    fn a_daytime_window_and_a_negative_offset() {
        // 12:00 to 14:00 in New York in winter (UTC-5). 17:30 UTC is 12:30 there.
        let prefs = quiet("12:00", "14:00", -300);
        assert!(prefs.in_quiet_hours(utc(17, 30)));
        assert!(!prefs.in_quiet_hours(utc(19, 30)));
    }

    #[test]
    fn switched_off_or_empty_windows_are_never_quiet() {
        let mut prefs = quiet("09:00", "09:00", 0);
        assert!(!prefs.in_quiet_hours(utc(9, 0)));
        prefs.quiet_to = "10:00".to_owned();
        prefs.quiet_hours = false;
        assert!(!prefs.in_quiet_hours(utc(9, 30)));
    }

    #[test]
    fn categories_channels_and_levels() {
        let prefs = NotificationPrefs::default();
        assert!(allows("reply", &prefs, None, Delivery::App));
        assert!(
            !allows("activity", &prefs, None, Delivery::Email),
            "never mailed by default"
        );
        assert!(!allows("reply", &prefs, Some("mentions"), Delivery::App));
        assert!(allows("mention", &prefs, Some("mentions"), Delivery::App));
        assert!(allows("direct", &prefs, Some("mentions"), Delivery::App));
        assert!(!allows("mention", &prefs, Some("none"), Delivery::App));
        assert!(
            allows("reminder", &prefs, Some("none"), Delivery::App),
            "asked for, it passes"
        );
        let no_mail = NotificationPrefs {
            email: false,
            ..NotificationPrefs::default()
        };
        assert!(!allows("mention", &no_mail, None, Delivery::Email));
        let off = NotificationPrefs {
            enabled: false,
            ..NotificationPrefs::default()
        };
        assert!(!allows("invitation", &off, None, Delivery::App));
        assert!(!allows("unknown", &prefs, None, Delivery::App));
    }

    #[test]
    fn do_not_disturb_and_quiet_hours_hold_interruptions() {
        let prefs = quiet("21:00", "08:00", 0);
        assert!(!may_interrupt(&prefs, None, utc(22, 0)));
        assert!(may_interrupt(&prefs, None, utc(12, 0)));
        assert!(!may_interrupt(&prefs, Some("dnd"), utc(12, 0)));
    }

    #[test]
    fn a_document_missing_fields_takes_the_defaults() {
        let read: NotificationPrefs = serde_json::from_str(r#"{ "sound": false }"#).unwrap();
        assert!(!read.sound);
        assert_eq!(read.app, Categories::default());
        assert!(quiet("25:00", "08:00", 0).validate().is_err());
        assert!(quiet("21:00", "08:00", 15 * 60).validate().is_err());
    }
}
