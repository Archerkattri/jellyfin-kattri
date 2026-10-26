use std::collections::BTreeSet;

use crate::I18nError;
use crate::catalog::Catalog;
use crate::locale::LocaleId;
use crate::template;

/// Text shown for a message id that no catalog in the chain has. Never the raw id.
pub const MISSING_TEXT: &str = "[?]";

/// Resolves message ids through a fallback chain built from the requested locale.
#[derive(Debug)]
pub struct Translator {
    chain: Vec<Catalog>,
    missing: BTreeSet<String>,
    seen_issues: BTreeSet<String>,
    unreported: Vec<String>,
}

impl Translator {
    /// Builds the chain for `requested` from the catalogs supplied. The en-US catalog must be
    /// among them; it is the last fallback for every locale.
    pub fn new(requested: &LocaleId, catalogs: &[Catalog]) -> Result<Self, I18nError> {
        let en = LocaleId::en_us();
        if !catalogs.iter().any(|catalog| catalog.locale() == &en) {
            return Err(I18nError::MissingFallback);
        }
        let chain = requested
            .fallback_chain()
            .iter()
            .filter_map(|wanted| {
                catalogs
                    .iter()
                    .find(|catalog| catalog.locale() == wanted)
                    .cloned()
            })
            .collect();
        Ok(Self {
            chain,
            missing: BTreeSet::new(),
            seen_issues: BTreeSet::new(),
            unreported: Vec::new(),
        })
    }

    /// Locales in lookup order, for the catalogs actually present.
    pub fn chain_locales(&self) -> Vec<&LocaleId> {
        self.chain.iter().map(Catalog::locale).collect()
    }

    /// The locale whose catalog supplies `id`, or `None` if no catalog has it.
    pub fn source_locale(&self, id: &str) -> Option<&LocaleId> {
        self.chain
            .iter()
            .find(|catalog| catalog.get(id).is_some())
            .map(Catalog::locale)
    }

    /// Returns the text for `id` with `args` substituted. A missing id yields [`MISSING_TEXT`]
    /// and is recorded. A missing argument stays as `{name}` and is recorded. Each distinct
    /// issue is queued once for [`take_new_issues`](Self::take_new_issues).
    pub fn text(&mut self, id: &str, args: &[(&str, &str)]) -> String {
        let found = self
            .chain
            .iter()
            .find_map(|catalog| catalog.get(id))
            .map(str::to_owned);
        let Some(template_text) = found else {
            self.missing.insert(id.to_owned());
            self.report(format!("missing message: {id}"));
            return MISSING_TEXT.to_owned();
        };
        match template::render(&template_text, args) {
            Ok(rendered) => {
                for name in &rendered.missing_args {
                    self.report(format!("missing argument `{name}` for message {id}"));
                }
                rendered.text
            }
            Err(_) => {
                self.report(format!("invalid template for message {id}"));
                MISSING_TEXT.to_owned()
            }
        }
    }

    /// Every message id that has been looked up and not found, sorted.
    pub fn missing_ids(&self) -> Vec<String> {
        self.missing.iter().cloned().collect()
    }

    /// Issues recorded since the last call, each distinct issue once. Intended for the caller's
    /// log. The strings carry ids and argument names, never argument values.
    pub fn take_new_issues(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unreported)
    }

    fn report(&mut self, issue: String) {
        if self.seen_issues.insert(issue.clone()) {
            self.unreported.push(issue);
        }
    }
}
