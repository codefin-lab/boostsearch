//! Telephone numbers, cut the way they are searched for.
//!
//! A number is written a dozen ways and searched for in a dozen more:
//! `+41 58 316 10 10`, `0583161010`, `41583161010`, or the first few digits
//! of any of them. Both ends are cut here so that they meet: the index keeps
//! the number, the number without its country code, and every prefix of it;
//! the search keeps the whole number only, so that a prefix typed at search
//! time is not matched against every number that starts the same way.
//!
//! This is OpenSearch's `analysis-phonenumber` plugin, reading numbers with
//! the same library it reads them with.

use std::collections::BTreeSet;
use std::str::FromStr;

/// Every token a number stands behind.
///
/// `ngrams` is what separates the two analyzers the plugin offers: `phone`
/// indexes with them, `phone-search` searches without them.
pub fn tokens(text: &str, region: &str, ngrams: bool) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut input = text.to_string();
    out.insert(input.clone());

    // a number written as a URI is a number with four characters in front
    if input.starts_with("tel:") || input.starts_with("sip:") {
        if ngrams {
            out.insert(input[..4].to_string());
        }
        input = input[4..].to_string();
    }

    // the whole of what is left, without a leading plus
    let start = usize::from(input.starts_with('+'));
    out.insert(input[start..].to_string());

    // what follows an @ is a host, not a number
    if let Some(at) = input.find('@') {
        input = input[..at].to_string();
        out.insert(input[start..].to_string());
    }

    let mut country: Option<String> = None;
    // `ZZ` is "no region named", which is what the library is asked to infer
    let country_hint = match region {
        "" | "ZZ" => None,
        other => phonenumber::country::Id::from_str(other).ok(),
    };
    if let Ok(number) = phonenumber::parse(country_hint, &input) {
        let code = number.code().value().to_string();
        let national = number.national().value().to_string();
        out.insert(format!("{code}{national}"));
        if ngrams {
            // the country code alone belongs in the index, where it narrows
            // nothing on its own, and not in a search, where it would match
            // every number in the country
            out.insert(code.clone());
            if let Some(ext) = number.extension() {
                out.insert(ext.as_ref().to_string());
            }
            out.insert(national.clone());
        }
        country = Some(code);
        input = national;
    }

    // every prefix of the number, so that a number typed as far as the caller
    // got is a number that matches
    if ngrams && !input.is_empty() && input.chars().all(|c| c.is_ascii_digit()) {
        for count in 1..=input.len() {
            let prefix = &input[..count];
            out.insert(prefix.to_string());
            if let Some(code) = &country {
                out.insert(format!("{code}{prefix}"));
            }
        }
    }
    out.into_iter().collect()
}
