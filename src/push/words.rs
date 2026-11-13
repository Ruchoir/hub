//! The words of an entry where no interface draws them: a phone's lock screen, a browser's tray and
//! the mail digest (design, decision 9.1).
//!
//! The interfaces word an entry from their own dictionaries. Here the hub holds a small set of
//! sentences per category, in the six languages, filled from what the product published in the
//! entry's `variables`:
//!
//! - `actor_name`: who did it, as their name reads (`Camille`);
//! - `place`: where, as the product names it (`#général · Acme`);
//! - `subject`: what it is about (an event's title, a shared file's name);
//! - `excerpt`: the first words of it, as written, never translated.
//!
//! Each is optional; a missing name reads "someone", a missing place "Ruchoir".

use serde_json::{Map, Value};

/// The languages the interface speaks. French is the source, and what a missing language falls back
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    #[default]
    Fr,
    En,
    Es,
    De,
    It,
    Pl,
}

impl Locale {
    /// Every language, French first.
    pub const ALL: [Locale; 6] = [
        Locale::Fr,
        Locale::En,
        Locale::Es,
        Locale::De,
        Locale::It,
        Locale::Pl,
    ];

    /// A language tag (`en`, `de-CH`, `pt`), or the first of an `Accept-Language` list, read as one
    /// of the six; anything else is French.
    pub fn parse(tag: Option<&str>) -> Self {
        let first = tag
            .unwrap_or("")
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match first.split(['-', '_', ';']).next().unwrap_or("") {
            "en" => Locale::En,
            "es" => Locale::Es,
            "de" => Locale::De,
            "it" => Locale::It,
            "pl" => Locale::Pl,
            _ => Locale::Fr,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Locale::Fr => "fr",
            Locale::En => "en",
            Locale::Es => "es",
            Locale::De => "de",
            Locale::It => "it",
            Locale::Pl => "pl",
        }
    }
}

fn text<'a>(variables: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    variables
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn someone(locale: Locale) -> &'static str {
    match locale {
        Locale::Fr => "Quelqu'un",
        Locale::En => "Someone",
        Locale::Es => "Alguien",
        Locale::De => "Jemand",
        Locale::It => "Qualcuno",
        Locale::Pl => "Ktoś",
    }
}

/// What someone did, after their name, for the categories that have someone.
fn verb(locale: Locale, category: &str) -> &'static str {
    match (locale, category) {
        (Locale::Fr, "mention") => "vous a mentionné",
        (Locale::Fr, "reply") => "vous a répondu",
        (Locale::Fr, "direct") => "vous a envoyé un message",
        (Locale::Fr, "invitation") => "vous a invité",
        (Locale::Fr, _) => "a publié du nouveau",
        (Locale::En, "mention") => "mentioned you",
        (Locale::En, "reply") => "replied to you",
        (Locale::En, "direct") => "sent you a message",
        (Locale::En, "invitation") => "invited you",
        (Locale::En, _) => "posted something new",
        (Locale::Es, "mention") => "te ha mencionado",
        (Locale::Es, "reply") => "te ha respondido",
        (Locale::Es, "direct") => "te ha enviado un mensaje",
        (Locale::Es, "invitation") => "te ha invitado",
        (Locale::Es, _) => "ha publicado algo nuevo",
        (Locale::De, "mention") => "hat Sie erwähnt",
        (Locale::De, "reply") => "hat Ihnen geantwortet",
        (Locale::De, "direct") => "hat Ihnen eine Nachricht geschickt",
        (Locale::De, "invitation") => "hat Sie eingeladen",
        (Locale::De, _) => "hat etwas Neues veröffentlicht",
        (Locale::It, "mention") => "ti ha menzionato",
        (Locale::It, "reply") => "ti ha risposto",
        (Locale::It, "direct") => "ti ha inviato un messaggio",
        (Locale::It, "invitation") => "ti ha invitato",
        (Locale::It, _) => "ha pubblicato qualcosa di nuovo",
        (Locale::Pl, "mention") => "wspomniał o Tobie",
        (Locale::Pl, "reply") => "odpowiedział Ci",
        (Locale::Pl, "direct") => "wysłał Ci wiadomość",
        (Locale::Pl, "invitation") => "zaprosił Cię",
        (Locale::Pl, _) => "opublikował coś nowego",
    }
}

/// The label of the categories that have no one behind them.
fn label(locale: Locale, category: &str) -> &'static str {
    match (locale, category) {
        (Locale::Fr, "reminder") => "Rappel",
        (Locale::Fr, _) => "Avis",
        (Locale::En, "reminder") => "Reminder",
        (Locale::En, _) => "Notice",
        (Locale::Es, "reminder") => "Recordatorio",
        (Locale::Es, _) => "Aviso",
        (Locale::De, "reminder") => "Erinnerung",
        (Locale::De, _) => "Hinweis",
        (Locale::It, "reminder") => "Promemoria",
        (Locale::It, _) => "Avviso",
        (Locale::Pl, "reminder") => "Przypomnienie",
        (Locale::Pl, _) => "Powiadomienie",
    }
}

/// Something shared with the person, its name quoted as each language quotes: "Camille a partagé
/// « Budget » avec vous". Without its name: "Camille a partagé quelque chose avec vous".
fn shared(locale: Locale, actor: &str, subject: Option<&str>) -> String {
    match (locale, subject) {
        // French typography: a non-breaking space inside the guillemets.
        (Locale::Fr, Some(s)) => format!("{actor} a partagé «\u{a0}{s}\u{a0}» avec vous"),
        (Locale::Fr, None) => format!("{actor} a partagé quelque chose avec vous"),
        (Locale::En, Some(s)) => format!("{actor} shared \u{201c}{s}\u{201d} with you"),
        (Locale::En, None) => format!("{actor} shared something with you"),
        (Locale::Es, Some(s)) => format!("{actor} ha compartido «{s}» contigo"),
        (Locale::Es, None) => format!("{actor} ha compartido algo contigo"),
        (Locale::De, Some(s)) => format!("{actor} hat \u{201e}{s}\u{201c} mit Ihnen geteilt"),
        (Locale::De, None) => format!("{actor} hat etwas mit Ihnen geteilt"),
        (Locale::It, Some(s)) => format!("{actor} ha condiviso «{s}» con te"),
        (Locale::It, None) => format!("{actor} ha condiviso qualcosa con te"),
        (Locale::Pl, Some(s)) => format!("{actor} udostępnił Ci \u{201e}{s}\u{201d}"),
        (Locale::Pl, None) => format!("{actor} udostępnił Ci coś"),
    }
}

/// What happened, in one line: "Camille vous a mentionné", "Rappel : Répétition".
pub fn headline(locale: Locale, category: &str, variables: &Map<String, Value>) -> String {
    if matches!(category, "reminder" | "notice") {
        let label = label(locale, category);
        return match text(variables, "subject") {
            // French typography: a non-breaking space before the colon.
            Some(subject) if locale == Locale::Fr => format!("{label}\u{a0}: {subject}"),
            Some(subject) => format!("{label}: {subject}"),
            None => label.to_owned(),
        };
    }
    let actor = text(variables, "actor_name").unwrap_or_else(|| someone(locale));
    if category == "share" {
        return shared(locale, actor, text(variables, "subject"));
    }
    format!("{actor} {}", verb(locale, category))
}

/// The line under the headline: the excerpt, or what an invitation is to.
pub fn detail(category: &str, variables: &Map<String, Value>) -> Option<String> {
    let excerpt = text(variables, "excerpt");
    let subject = text(variables, "subject").filter(|_| category == "invitation");
    excerpt.or(subject).map(str::to_owned)
}

/// An entry as a tray or a lock screen draws it: the place as the title, then what happened and its
/// first words.
pub fn tray(locale: Locale, category: &str, variables: &Map<String, Value>) -> (String, String) {
    let title = text(variables, "place").unwrap_or("Ruchoir").to_owned();
    let headline = headline(locale, category, variables);
    let body = match detail(category, variables) {
        Some(detail) => format!("{headline}\n{detail}"),
        None => headline,
    };
    (title, body)
}

/// An entry as a line of the mail digest: what happened and where, then its first words.
pub fn digest_line(
    locale: Locale,
    category: &str,
    variables: &Map<String, Value>,
) -> (String, String) {
    let headline = headline(locale, category, variables);
    let headline = match text(variables, "place") {
        Some(place) => format!("{headline} · {place}"),
        None => headline,
    };
    (headline, detail(category, variables).unwrap_or_default())
}

/// The body of the test push sent from the preferences.
pub fn test_body(locale: Locale) -> &'static str {
    match locale {
        Locale::Fr => "Les notifications arrivent bien sur cet appareil, même Ruchoir fermé.",
        Locale::En => "Notifications reach this device, even with Ruchoir closed.",
        Locale::Es => "Las notificaciones llegan a este dispositivo, incluso con Ruchoir cerrado.",
        Locale::De => {
            "Benachrichtigungen erreichen dieses Gerät, auch wenn Ruchoir geschlossen ist."
        }
        Locale::It => "Le notifiche arrivano su questo dispositivo, anche con Ruchoir chiuso.",
        Locale::Pl => "Powiadomienia docierają na to urządzenie, nawet gdy Ruchoir jest zamknięty.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_language_tag_is_read_as_one_of_the_six() {
        assert_eq!(Locale::parse(Some("en-GB,en;q=0.9,fr;q=0.8")), Locale::En);
        assert_eq!(Locale::parse(Some("de_CH")), Locale::De);
        assert_eq!(Locale::parse(Some("pt-BR")), Locale::Fr);
        assert_eq!(Locale::parse(None), Locale::Fr);
    }

    #[test]
    fn someone_did_something_somewhere() {
        let v = vars(
            json!({ "actor_name": "Camille", "place": "#général · Acme", "excerpt": "On répète jeudi ?" }),
        );
        assert_eq!(
            tray(Locale::Fr, "mention", &v),
            (
                "#général · Acme".to_owned(),
                "Camille vous a mentionné\nOn répète jeudi ?".to_owned()
            )
        );
        assert_eq!(
            digest_line(Locale::En, "mention", &v),
            (
                "Camille mentioned you · #général · Acme".to_owned(),
                "On répète jeudi ?".to_owned()
            )
        );
    }

    #[test]
    fn what_is_missing_reads_plainly() {
        let empty = Map::new();
        assert_eq!(
            tray(Locale::De, "direct", &empty),
            (
                "Ruchoir".to_owned(),
                "Jemand hat Ihnen eine Nachricht geschickt".to_owned()
            )
        );
        assert_eq!(headline(Locale::It, "reminder", &empty), "Promemoria");
        let v = vars(json!({ "subject": "Répétition" }));
        assert_eq!(
            headline(Locale::Fr, "reminder", &v),
            "Rappel\u{a0}: Répétition"
        );
        assert_eq!(headline(Locale::En, "notice", &v), "Notice: Répétition");
        // An invitation says what it is to; a mention does not repeat its subject.
        assert_eq!(detail("invitation", &v).as_deref(), Some("Répétition"));
        assert_eq!(detail("mention", &v), None);
    }

    #[test]
    fn a_share_names_who_and_what() {
        let v = vars(json!({ "actor_name": "Camille", "subject": "Budget", "place": "Acme" }));
        assert_eq!(
            digest_line(Locale::Fr, "share", &v),
            (
                "Camille a partagé «\u{a0}Budget\u{a0}» avec vous · Acme".to_owned(),
                String::new()
            )
        );
        assert_eq!(
            headline(Locale::En, "share", &v),
            "Camille shared \u{201c}Budget\u{201d} with you"
        );
        let v = vars(json!({ "subject": "Budget" }));
        assert_eq!(
            headline(Locale::Fr, "share", &v),
            "Quelqu'un a partagé «\u{a0}Budget\u{a0}» avec vous"
        );
        assert_eq!(
            headline(Locale::De, "share", &Map::new()),
            "Jemand hat etwas mit Ihnen geteilt"
        );
    }

    #[test]
    fn every_category_has_words_in_every_language() {
        let v = vars(json!({ "actor_name": "A", "subject": "S" }));
        for category in crate::inbox::CATEGORIES {
            for locale in Locale::ALL {
                let line = headline(locale, category, &v);
                assert!(
                    !line.is_empty() && !line.contains('{'),
                    "{category} {locale:?}"
                );
            }
        }
    }
}
