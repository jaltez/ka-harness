//! ka passport: subscription OAuth for ka's passport tier.
//!
//! The one auth abstraction ([`ka_dialect::auth::TokenSource`], ratified
//! research/ka-harness/sdk.md §6) gains its first end-to-end provider:
//! Claude Pro/Max plan OAuth (PKCE S256, loopback callback, proactive
//! refresh), tokens in the OS keyring under service `ka-passport` with
//! an in-memory store for tests. Core never links this crate —
//! ka-harness opts in through its `passport` cargo feature, whose
//! activation also flips the ka-dialect registry consult on
//! (`ka login claude` / `ka logout claude` / `ka passport status` are
//! the integrator's CLI seams over [`ClaudePlanSource`]).
//!
//! ```no_run
//! // harness startup (feature "passport"):
//! let passport = ka_passport::install();   // registers the source
//! // later, from the turn machinery:
//! let token = ka_dialect::auth::resolve_passport_token("claude-plan");
//! ```

pub mod claude;
pub mod pkce;
pub mod store;

pub use claude::{ClaudePlanSource, Status, Tokens, VENDOR, install};
pub use pkce::Pkce;
pub use store::{KeyringStore, MemoryStore, TokenStore};

// ka-dialect re-exports: the trait this crate implements plus the wire
// constants it must stay in lockstep with.
pub use ka_dialect::auth::{
    ANTHROPIC_OAUTH_BETA, AuthError, PASSPORT_KEYRING_SERVICE, TokenSource,
};
