//! The Snowball stemmers BoostCore does not carry.
//!
//! Six languages -- Catalan, Basque, Irish, Lithuanian, Estonian and Armenian
//! -- and the original Porter algorithm are generated from the Snowball
//! definitions by the Snowball compiler, which emits Rust. The generated code
//! and the small runtime it stands on are the Snowball project's own, under
//! the BSD 3-clause licence:
//!
//! > Copyright (c) 2001, Dr Martin Porter; (c) 2004,2005, Richard Boulton;
//! > (c) 2013, Yoshiki Shibukawa; (c) 2006-2025, Olly Betts. All rights
//! > reserved. Redistribution and use in source and binary forms, with or
//! > without modification, are permitted provided that the conditions in
//! > `LICENSE-SNOWBALL` are met.
//!
//! Reading them is not the point; they say exactly what the algorithms say,
//! which is what a query and the document it looks for both need.

// The algorithms below are generated, and are left exactly as the compiler
// wrote them so that regenerating them is a copy rather than an edit.
#![allow(clippy::all)]

mod among;
mod snowball_env;

pub use among::Among;
pub use snowball_env::SnowballEnv;

mod armenian;
mod basque;
mod catalan;
mod estonian;
mod irish;
mod lithuanian;
mod porter;

/// One word, as the named algorithm leaves it.
pub fn stem(language: &str, word: &str) -> Option<String> {
    let mut env = SnowballEnv::create(word);
    let ran = match language {
        "armenian" => armenian::stem(&mut env),
        "basque" => basque::stem(&mut env),
        "catalan" => catalan::stem(&mut env),
        "estonian" => estonian::stem(&mut env),
        "irish" => irish::stem(&mut env),
        "lithuanian" => lithuanian::stem(&mut env),
        "porter" => porter::stem(&mut env),
        _ => return None,
    };
    let _ = ran;
    Some(env.get_current().into_owned())
}
