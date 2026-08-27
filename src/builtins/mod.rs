//! Job implementations available to the scheduler.
//!
//! [`shell`] provides the generic command runner used by most jobs; [`lastfm`]
//! holds the built-ins that need typed access to the Last.fm API.

pub mod lastfm;
pub mod shell;
