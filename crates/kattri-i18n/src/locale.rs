use std::fmt;

use crate::I18nError;

/// A locale tag: `language` or `language-REGION`. Language is lowercased, region uppercased.
/// Accepts `-` or `_` as the separator; anything with a further subtag is rejected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocaleId {
    language: String,
    region: Option<String>,
}

impl LocaleId {
    pub fn parse(tag: &str) -> Result<Self, I18nError> {
        let invalid = || I18nError::InvalidLocale(tag.to_owned());
        let mut parts = tag.split(['-', '_']);
        let language = match parts.next() {
            Some(lang)
                if (2..=3).contains(&lang.len())
                    && lang.chars().all(|c| c.is_ascii_alphabetic()) =>
            {
                lang.to_ascii_lowercase()
            }
            _ => return Err(invalid()),
        };
        let region = match parts.next() {
            None => None,
            Some(region)
                if region.len() == 2 && region.chars().all(|c| c.is_ascii_alphabetic()) =>
            {
                Some(region.to_ascii_uppercase())
            }
            Some(_) => return Err(invalid()),
        };
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self { language, region })
    }

    /// The en-US locale, the mandatory last fallback.
    pub fn en_us() -> Self {
        Self {
            language: "en".to_owned(),
            region: Some("US".to_owned()),
        }
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// The base language without region (`de-AT` becomes `de`).
    pub fn base(&self) -> Self {
        Self {
            language: self.language.clone(),
            region: None,
        }
    }

    /// Lookup order for this locale: the locale itself, its base language, then en-US, which is
    /// always last. `en-US` yields only itself, `fr` yields `fr` then `en-US`, and `de-AT` yields
    /// `de-AT`, `de`, `en-US`.
    pub fn fallback_chain(&self) -> Vec<Self> {
        let en = Self::en_us();
        if *self == en {
            return vec![en];
        }
        let mut chain = vec![self.clone()];
        if self.region.is_some() {
            chain.push(self.base());
        }
        chain.push(en);
        chain
    }
}

impl fmt::Display for LocaleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.region {
            Some(region) => write!(f, "{}-{}", self.language, region),
            None => f.write_str(&self.language),
        }
    }
}
