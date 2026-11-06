//! The words `panel.rs` and its neighbours put in front of a user, in the user's
//! language.
//!
//! English is the key: `tr("Today")` looks the sentence up, and a sentence with no
//! entry reads in English rather than failing. Placeholders are named, because a
//! translation can reorder them: `tf!("Resets {reset}", reset = when)`.
//! `tests::every_sentence_has_a_translation` reads the sources this feeds, so a new
//! string with no French fails a test rather than shipping half translated.
//!
//! Only content goes through here. Logs, `--doctor`, errors a provider wrote and the
//! CLI's own output stay English.
use std::cell::Cell;
use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};

use chrono::{Datelike, Weekday};
use serde::{Deserialize, Serialize};

/// `[panel] language`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LanguageSetting {
    #[default]
    Auto,
    En,
    Fr,
}

impl LanguageSetting {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::En => "en",
            Self::Fr => "fr",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    En,
    Fr,
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

thread_local! {
    static OVERRIDE: Cell<Option<Language>> = const { Cell::new(None) };
}

fn language_of_tag(tag: &str) -> Option<Language> {
    let primary = tag.split(['_', '-', '.', '@']).next()?.to_ascii_lowercase();
    match primary.as_str() {
        "" => None,
        "fr" => Some(Language::Fr),
        _ => Some(Language::En),
    }
}

#[cfg(any(windows, target_os = "macos"))]
fn system_tag() -> Option<String> {
    sys_locale::get_locale()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_tag() -> Option<String> {
    None
}

/// A GUI process on macOS or Windows usually has none of these set, which is what
/// the system's own answer is for.
fn detected() -> Language {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|tag| !tag.is_empty()))
        .or_else(system_tag)
        .as_deref()
        .and_then(language_of_tag)
        .unwrap_or(Language::En)
}

/// Called wherever a config is loaded, so a daemon's reload moves the language too.
pub fn apply(setting: LanguageSetting) {
    let language = match setting {
        LanguageSetting::Auto => detected(),
        LanguageSetting::En => Language::En,
        LanguageSetting::Fr => Language::Fr,
    };
    CURRENT.store(language as u8, Ordering::Relaxed);
}

pub fn current() -> Language {
    if let Some(language) = OVERRIDE.with(Cell::get) {
        return language;
    }
    // A test reads English whatever locale the developer runs it under.
    if cfg!(test) || CURRENT.load(Ordering::Relaxed) == Language::En as u8 {
        Language::En
    } else {
        Language::Fr
    }
}

/// Render in `language` on this thread alone.
pub fn with_language<T>(language: Language, body: impl FnOnce() -> T) -> T {
    let previous = OVERRIDE.with(|cell| cell.replace(Some(language)));
    let out = body();
    OVERRIDE.with(|cell| cell.set(previous));
    out
}

pub fn tr(english: &str) -> &str {
    match current() {
        Language::En => english,
        Language::Fr => FRENCH
            .iter()
            .find(|(en, _)| *en == english)
            .map_or(english, |(_, fr)| fr),
    }
}

pub fn fill(template: &str, values: &[(&str, &dyn Display)]) -> String {
    let mut out = template.to_string();
    for (name, value) in values {
        out = out.replace(&format!("{{{name}}}"), &value.to_string());
    }
    out
}

macro_rules! tf {
    ($english:literal $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::tr($english),
            &[$((stringify!($name), &$value as &dyn ::std::fmt::Display)),*],
        )
    };
}
pub(crate) use tf;

const MONTHS: [&str; 12] = [
    "janvier",
    "février",
    "mars",
    "avril",
    "mai",
    "juin",
    "juillet",
    "août",
    "septembre",
    "octobre",
    "novembre",
    "décembre",
];
const MONTHS_SHORT: [&str; 12] = [
    "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.",
    "déc.",
];

fn weekday_fr(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "lundi",
        Weekday::Tue => "mardi",
        Weekday::Wed => "mercredi",
        Weekday::Thu => "jeudi",
        Weekday::Fri => "vendredi",
        Weekday::Sat => "samedi",
        Weekday::Sun => "dimanche",
    }
}

fn weekday_short_fr(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "lun.",
        Weekday::Tue => "mar.",
        Weekday::Wed => "mer.",
        Weekday::Thu => "jeu.",
        Weekday::Fri => "ven.",
        Weekday::Sat => "sam.",
        Weekday::Sun => "dim.",
    }
}

/// A `strftime` pattern with its month and weekday names already in the language,
/// since chrono's own are English. Hand the result to `.format()`.
pub fn date_pattern(pattern: &str, date: &impl Datelike) -> String {
    if current() == Language::En {
        return pattern.to_string();
    }
    let month = date.month0() as usize;
    pattern
        .replace("%B", MONTHS[month])
        .replace("%b", MONTHS_SHORT[month])
        .replace("%A", weekday_fr(date.weekday()))
        .replace("%a", weekday_short_fr(date.weekday()))
}

/// The one-letter unit a duration is written in: `2d 8h` is `2j 8h` in French.
pub fn day_unit() -> &'static str {
    match current() {
        Language::En => "d",
        Language::Fr => "j",
    }
}

const FRENCH: &[(&str, &str)] = &[
    // Section titles
    ("STATUS", "ÉTAT"),
    ("LIMITS", "LIMITES"),
    ("COST", "COÛT"),
    ("TOKENS BY DAY", "TOKENS PAR JOUR"),
    (
        "TOKENS BY MODEL · THIS MONTH",
        "TOKENS PAR MODÈLE · CE MOIS-CI",
    ),
    (
        "TOKENS BY DEVICE · THIS MONTH",
        "TOKENS PAR APPAREIL · CE MOIS-CI",
    ),
    ("ALL PLANS", "TOUS LES FORFAITS"),
    (
        "ALL PLANS · {count} credentials",
        "TOUS LES FORFAITS · {count} identifiants",
    ),
    // Devices
    ("{usd} · partial", "{usd} · partiel"),
    ("  · partial", "  · partiel"),
    ("This machine", "Cette machine"),
    ("Last published  {when}", "Dernière publication  {when}"),
    ("Tokens  {count}", "Tokens  {count}"),
    (
        "Joined the fleet part-way through the month, so its share is only what it has covered",
        "A rejoint la flotte en cours de mois : sa part se limite à ce qu’elle couvre",
    ),
    // Time
    ("{label} · all plans", "{label} · tous les forfaits"),
    ("Last refresh unknown", "Dernière actualisation inconnue"),
    (
        "Last refreshed {when} · {stamp}",
        "Actualisé {when} · {stamp}",
    ),
    ("just now", "à l’instant"),
    ("{count}m ago", "il y a {count} min"),
    ("{count}h ago", "il y a {count} h"),
    ("{count}d ago", "il y a {count} j"),
    ("in {duration}", "dans {duration}"),
    ("now", "maintenant"),
    // Status
    (
        "the last live fetch failed",
        "la dernière récupération a échoué",
    ),
    ("Stale", "Périmé"),
    ("Stale · {name}", "Périmé · {name}"),
    (
        "Showing the last figures that arrived.\n{reason}",
        "Derniers chiffres reçus.\n{reason}",
    ),
    // Limits
    ("not started", "pas commencé"),
    ("Resets {reset}", "Réinitialisation {reset}"),
    (", expires in {left}", ", expire dans {left}"),
    (
        "{count} reset banked{expiry}",
        "{count} réinitialisation en réserve{expiry}",
    ),
    (
        "{count} resets banked{expiry}",
        "{count} réinitialisations en réserve{expiry}",
    ),
    ("live login", "connexion active"),
    ("active", "actif"),
    ("not in total", "hors total"),
    ("{shown} of {total} shown", "{shown} sur {total} affichés"),
    ("Limits", "Limites"),
    ("none reported", "aucune signalée"),
    ("Expired", "Expiré"),
    ("remuda refreshes it", "remuda le renouvelle"),
    (
        "Its access token has expired, so it was not asked about. remuda's timer refreshes stored credentials every 30 minutes; `remuda refresh` does it now.",
        "Son jeton d’accès a expiré, il n’a donc pas été interrogé. Le minuteur de remuda renouvelle les identifiants stockés toutes les 30 minutes ; `remuda refresh` le fait tout de suite.",
    ),
    ("Unverified", "Non vérifié"),
    ("remuda re-identifies it", "remuda le réidentifie"),
    (
        "Its sidecar was written for other tokens, so whose they are is unknown. remuda re-identifies it on its next run.",
        "Son fichier compagnon a été écrit pour d’autres jetons, on ne sait donc pas à qui ils sont. remuda le réidentifie à sa prochaine exécution.",
    ),
    ("Unknown state", "État inconnu"),
    (
        "A newer build wrote a state this one cannot draw.",
        "Une version plus récente a écrit un état que celle-ci ne sait pas afficher.",
    ),
    ("not asked", "non interrogé"),
    ("estimate", "estimation"),
    (
        "Weighted by each plan's nominal multiplier. The real limits are not published, so this is an estimate.",
        "Pondéré par le multiplicateur nominal de chaque forfait. Les limites réelles ne sont pas publiées : c’est une estimation.",
    ),
    (
        "Every plan counts as 100%, whatever its size.",
        "Chaque forfait compte pour 100 %, quelle que soit sa taille.",
    ),
    ("Not in the total: {names}", "Hors total : {names}"),
    // Window labels
    ("Session", "Session"),
    ("Weekly", "Hebdo"),
    ("Monthly", "Mensuel"),
    ("Tertiary", "Tertiaire"),
    ("30-day", "30 jours"),
    ("5-hour", "5 heures"),
    ("Credit", "Crédit"),
    ("Key limit", "Limite de clé"),
    ("Balance", "Solde"),
    ("Included", "Inclus"),
    ("Cursor models", "Modèles Cursor"),
    ("Other models", "Autres modèles"),
    ("Weekly (all)", "Hebdo (tout)"),
    ("Weekly (Sonnet)", "Hebdo (Sonnet)"),
    ("On-demand", "À la demande"),
    ("Rate Limit", "Limite de débit"),
    // Cost
    ("Credits", "Crédits"),
    ("of {cap}", "sur {cap}"),
    ("{percent}% used", "{percent} % utilisé"),
    ("{title} resets {when}", "{title} : réinitialisation {when}"),
    ("Plan limit", "Limite du forfait"),
    ("On-demand cap", "Plafond à la demande"),
    ("Spend limit", "Limite de dépenses"),
    ("Sync", "Synchro"),
    ("1 device", "1 appareil"),
    ("{count} devices", "{count} appareils"),
    (
        "Cost and token figures cover every machine in the fleet",
        "Les coûts et les tokens couvrent toutes les machines de la flotte",
    ),
    ("Today", "Aujourd’hui"),
    ("{count} tokens", "{count} tokens"),
    (
        "{arrow}{percent}% vs prior avg",
        "{arrow}{percent} % par rapport à la moyenne",
    ),
    ("7-day", "7 jours"),
    ("This month", "Ce mois-ci"),
    ("Burn rate", "Rythme de dépense"),
    ("{usd}/hr", "{usd}/h"),
    ("By device", "Par appareil"),
    ("By model", "Par modèle"),
    ("Input   {count}", "Entrée  {count}"),
    ("Output  {count}", "Sortie  {count}"),
    ("Cache write  {count}", "Écriture du cache  {count}"),
    ("Cache read   {count}", "Lecture du cache   {count}"),
    ("{usd} this month", "{usd} ce mois-ci"),
    // History
    ("30 days", "30 jours"),
    ("90 days", "90 jours"),
    ("12 months", "12 mois"),
    (
        "months before {earliest} are rated at today's prices; this build's price history does not reach further",
        "les mois avant {earliest} sont valorisés aux prix actuels : l’historique des prix de cette version ne remonte pas plus loin",
    ),
    ("{names} and {rest} more", "{names} et {rest} de plus"),
    (
        "no price for {listed}: their tokens are counted, their cost is not",
        "aucun prix pour {listed} : leurs tokens sont comptés, pas leur coût",
    ),
    ("since {date}", "depuis le {date}"),
    ("no history yet", "pas encore d’historique"),
    // Notifications
    (
        "TokenGauge: {provider} {window} at {percent}%",
        "TokenGauge : {provider} {window} à {percent} %",
    ),
    ("resets {reset}", "réinitialisation {reset}"),
    (
        "TokenGauge: update available",
        "TokenGauge : mise à jour disponible",
    ),
    (
        "v{latest} is available (you have v{current}). {how}",
        "v{latest} est disponible (vous avez la v{current}). {how}",
    ),
    ("Run tokengauge --update.", "Lancez tokengauge --update."),
    ("Upgrade it with {by}.", "Mettez-la à jour avec {by}."),
    // Pace
    ("empty in {eta}", "vide dans {eta}"),
    ("ends ~{percent}%", "fin ~{percent} %"),
];

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCES: &[&str] = &[
        "panel.rs",
        "history.rs",
        "pace.rs",
        "fmt.rs",
        "rows.rs",
        "../../tokengauge-waybar/src/snapshot.rs",
        "../../tokengauge-waybar/src/daemon.rs",
    ];

    fn sentences_in(source: &str) -> Vec<String> {
        let body = source.split("#[cfg(test)]").next().unwrap_or(source);
        let mut out = Vec::new();
        for opener in ["tr(\"", "tf!(\""] {
            for piece in body.split(opener).skip(1) {
                let mut sentence = String::new();
                let mut chars = piece.chars();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => sentence.push('\n'),
                            Some(other) => sentence.push(other),
                            None => {}
                        },
                        other => sentence.push(other),
                    }
                }
                out.push(sentence);
            }
        }
        out
    }

    fn every_sentence() -> Vec<String> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        SOURCES
            .iter()
            .flat_map(|file| sentences_in(&std::fs::read_to_string(dir.join(file)).unwrap()))
            .collect()
    }

    #[test]
    fn every_sentence_has_a_translation() {
        let sentences = every_sentence();
        assert!(sentences.len() > 60, "the scan found {}", sentences.len());
        for sentence in &sentences {
            assert!(
                FRENCH.iter().any(|(en, _)| en == sentence),
                "no French for {sentence:?}"
            );
        }
    }

    #[test]
    fn every_provider_window_has_a_translation() {
        for meta in crate::PROVIDER_META {
            let (session, weekly, tertiary) = meta.windows;
            for label in [session, weekly, tertiary] {
                assert!(
                    FRENCH.iter().any(|(en, _)| *en == label),
                    "no French for {}'s {label:?}",
                    meta.id
                );
            }
        }
    }

    #[test]
    fn a_translation_keeps_every_placeholder_and_no_english_is_listed_twice() {
        let placeholders = |text: &str| {
            let mut names: Vec<String> = text
                .split('{')
                .skip(1)
                .filter_map(|rest| rest.split('}').next().map(str::to_string))
                .collect();
            names.sort();
            names
        };
        let mut seen = std::collections::HashSet::new();
        for (en, fr) in FRENCH {
            assert!(seen.insert(*en), "{en:?} is listed twice");
            assert_eq!(placeholders(en), placeholders(fr), "{en:?}");
        }
    }

    #[test]
    fn a_locale_tag_names_its_language() {
        assert_eq!(language_of_tag("fr_FR.UTF-8"), Some(Language::Fr));
        assert_eq!(language_of_tag("fr-CA"), Some(Language::Fr));
        assert_eq!(language_of_tag("fr"), Some(Language::Fr));
        assert_eq!(language_of_tag("en_US.UTF-8"), Some(Language::En));
        assert_eq!(language_of_tag("de_DE"), Some(Language::En));
        assert_eq!(language_of_tag("C"), Some(Language::En));
        assert_eq!(language_of_tag(""), None);
    }

    #[test]
    fn a_sentence_is_looked_up_on_the_thread_that_asked() {
        assert_eq!(tr("Today"), "Today");
        with_language(Language::Fr, || {
            assert_eq!(tr("Today"), "Aujourd’hui");
            assert_eq!(
                tf!("Resets {reset}", reset = "dans 2h"),
                "Réinitialisation dans 2h"
            );
        });
        assert_eq!(tr("Today"), "Today");
    }

    #[test]
    fn a_date_is_named_in_the_language() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();
        let render = |pattern| date.format(&date_pattern(pattern, &date)).to_string();
        assert_eq!(render("%A %-d %B"), "Monday 2 November");
        with_language(Language::Fr, || {
            assert_eq!(render("%A %-d %B"), "lundi 2 novembre");
            assert_eq!(render("%a"), "lun.");
            assert_eq!(render("%-d %b %Y"), "2 nov. 2026");
        });
    }
}
