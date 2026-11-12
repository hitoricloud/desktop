//! UI text: `t!("area.key")`, `t!("area.key", name = value)`, plurals with the count first as `n = count`. build.rs makes
//! one `t!` arm per key in `locales/en/<area>.toml` (a wrong key or argument does not compile); missing ones fall back to English.
use std::fmt::{Display, Write};
use std::sync::atomic::{AtomicU8, Ordering};

include!(concat!(env!("OUT_DIR"), "/i18n.rs"));

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Ru,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::En, Lang::Ru];

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
        }
    }

    /// `ru-RU`, `ru_RU.UTF-8`, `en` → the language, if the app has it.
    pub fn parse(tag: &str) -> Option<Lang> {
        let lang = tag.split(['-', '_', '.', '@']).next()?.to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| l.code() == lang)
    }

    /// Its own name, the same in every language.
    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Русский",
        }
    }

    /// CLDR plural form of an integer count: `ONE`, `FEW`, `MANY` or `OTHER`.
    fn form(self, n: u64) -> usize {
        match self {
            Lang::En => if n == 1 { ONE } else { OTHER },
            Lang::Ru => match (n % 10, n % 100) {
                (1, h) if h != 11 => ONE,
                (2..=4, h) if !(12..=14).contains(&h) => FEW,
                _ => MANY,
            },
        }
    }
}

const ONE: usize = 0;
const FEW: usize = 1;
const MANY: usize = 2;
const OTHER: usize = 3;

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn current() -> Lang {
    Lang::ALL[CURRENT.load(Ordering::Relaxed) as usize]
}

pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// The first of the user's preferred languages (LANGUAGE, LC_ALL, LC_MESSAGES, LANG; Windows and macOS settings) the app has.
pub fn system() -> Lang {
    sys_locale::get_locales().find_map(|tag| Lang::parse(&tag)).unwrap_or(Lang::En)
}

/// The saved choice: `None` follows the system.
pub fn resolve(choice: Option<Lang>) -> Lang {
    choice.unwrap_or_else(system)
}

fn template(i: usize, n: Option<u64>) -> &'static str {
    let pick = |lang: Lang| {
        let forms = &TEXT[lang as usize][i];
        n.and_then(|n| forms[lang.form(n)]).or(forms[OTHER])
    };
    pick(current()).or_else(|| pick(Lang::En)).unwrap_or(KEYS[i])
}

pub fn text(i: usize) -> &'static str {
    template(i, None)
}

pub fn format(i: usize, n: Option<u64>, args: &[&dyn Display]) -> String {
    let mut out = String::new();
    let mut rest = template(i, n);
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        let arg = rest.find('}').and_then(|close| ARGS[i].iter().position(|a| *a == &rest[..close]).map(|p| (close, p)));
        match arg {
            Some((close, p)) => {
                let _ = write!(out, "{}", args[p]);
                rest = &rest[close + 1..];
            }
            None => out.push('{'),
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<&str> {
        let mut found: Vec<&str> = text.split('{').skip(1).filter_map(|s| s.split_once('}').map(|(name, _)| name)).collect();
        found.sort_unstable();
        found.dedup();
        found
    }

    #[test]
    fn every_language_has_every_key_and_the_same_placeholders() {
        assert!(UNKNOWN.is_empty(), "keys missing from locales/en: {UNKNOWN:?}");
        let mut problems = vec![];
        for lang in Lang::ALL {
            let needed: &[usize] = match lang {
                Lang::En => &[ONE, OTHER],
                Lang::Ru => &[ONE, FEW, MANY],
            };
            for (i, key) in KEYS.iter().enumerate() {
                let (en, forms) = (&TEXT[0][i], &TEXT[lang as usize][i]);
                let plural = en[ONE].is_some();
                let want: Vec<&str> = en.iter().flatten().flat_map(|t| names(t)).collect::<std::collections::BTreeSet<_>>().into_iter().filter(|n| !plural || *n != "n").collect();
                if forms.iter().all(Option::is_none) {
                    problems.push(format!("{}: {key} is missing", lang.code()));
                    continue;
                }
                if plural && let Some(f) = needed.iter().find(|f| forms[**f].is_none()) {
                    problems.push(format!("{}: {key} has no {} form", lang.code(), ["one", "few", "many", "other"][*f]));
                }
                if !plural && forms[OTHER].is_none() {
                    problems.push(format!("{}: {key} should be a plain string", lang.code()));
                }
                for text in forms.iter().flatten() {
                    let got: Vec<&str> = names(text).into_iter().filter(|n| !plural || *n != "n").collect();
                    if got != want {
                        problems.push(format!("{}: {key} has placeholders {got:?}, English has {want:?}", lang.code()));
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn russian_plural_forms() {
        let forms: Vec<usize> = [0, 1, 2, 4, 5, 11, 12, 14, 21, 22, 25, 101, 111, 1000].map(|n| Lang::Ru.form(n)).to_vec();
        assert_eq!(forms, [MANY, ONE, FEW, FEW, MANY, MANY, MANY, MANY, ONE, FEW, MANY, ONE, MANY, MANY]);
        assert_eq!([0, 1, 2].map(|n| Lang::En.form(n)), [OTHER, ONE, OTHER]);
    }

    #[test]
    fn locale_tags() {
        assert_eq!(Lang::parse("ru_RU.UTF-8"), Some(Lang::Ru));
        assert_eq!(Lang::parse("en-US"), Some(Lang::En));
        assert_eq!(Lang::parse("C"), None);
        assert_eq!(Lang::parse("de-DE"), None);
    }
}
