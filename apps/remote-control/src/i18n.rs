//! Minimal zero-dependency UI language support (Chinese / English).
//! Defaults to the Windows user UI language; overridden by `settings.txt`
//! (`language=auto|zh|en`) or `RC_LANG=zh|en` (useful for testing/screenshots).

use std::{
    path::Path,
    sync::atomic::{AtomicU8, Ordering},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LangPref {
    #[default]
    Auto,
    Zh,
    En,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl LangPref {
    pub fn as_str(self) -> &'static str {
        match self {
            LangPref::Auto => "auto",
            LangPref::Zh => "zh",
            LangPref::En => "en",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh-tw" | "zh-hk" | "zh-sg" => LangPref::Zh,
            "en" | "en-us" | "en-gb" => LangPref::En,
            _ => LangPref::Auto,
        }
    }

    fn to_u8(self) -> u8 {
        match self {
            LangPref::Auto => 0,
            LangPref::Zh => 1,
            LangPref::En => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => LangPref::Zh,
            2 => LangPref::En,
            _ => LangPref::Auto,
        }
    }
}

static PREF: AtomicU8 = AtomicU8::new(0);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

pub fn system_lang() -> Lang {
    // Primary language ID is the low 10 bits; LANG_CHINESE = 0x04.
    let lang_id = unsafe { GetUserDefaultUILanguage() };
    if (lang_id & 0x03ff) == 0x04 { Lang::Zh } else { Lang::En }
}

pub fn set_pref(pref: LangPref) {
    PREF.store(pref.to_u8(), Ordering::Relaxed);
}

pub fn pref() -> LangPref {
    LangPref::from_u8(PREF.load(Ordering::Relaxed))
}

pub fn lang() -> Lang {
    if let Ok(v) = std::env::var("RC_LANG") {
        match v.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh-tw" => return Lang::Zh,
            "en" | "en-us" | "en-gb" => return Lang::En,
            _ => {}
        }
    }
    match pref() {
        LangPref::Auto => system_lang(),
        LangPref::Zh => Lang::Zh,
        LangPref::En => Lang::En,
    }
}

#[inline]
pub fn is_en() -> bool {
    lang() == Lang::En
}

#[inline]
pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// Load `language=` from the settings file next to `key` and apply it.
pub fn init_from_key(key: &Path) {
    let settings = crate::store::Settings::load(&crate::store::settings_path(key));
    set_pref(settings.language);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pref_round_trip() {
        assert_eq!(LangPref::parse("auto"), LangPref::Auto);
        assert_eq!(LangPref::parse("zh"), LangPref::Zh);
        assert_eq!(LangPref::parse("ZH-CN"), LangPref::Zh);
        assert_eq!(LangPref::parse("en"), LangPref::En);
        assert_eq!(LangPref::parse("EN-US"), LangPref::En);
        assert_eq!(LangPref::parse("unknown"), LangPref::Auto);
        for p in [LangPref::Auto, LangPref::Zh, LangPref::En] {
            assert_eq!(LangPref::parse(p.as_str()), p);
        }
    }
}
