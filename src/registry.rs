//! The built-in job registry.
//!
//! Built-ins are looked up by the name written in the jobs file. Registration
//! is conditional: the Last.fm jobs only appear when the Last.fm environment is
//! configured, so an install that never touches Last.fm is not obliged to
//! provide credentials it will not use.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::builtins::lastfm::{CurrentTrack, LastFm, RecentPlays, ScrobblesDb, TopTracksGist};
use crate::job::Job;

/// Built-in jobs available to the jobs file, keyed by name.
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, Arc<dyn Job>>,
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds every Last.fm built-in, sharing one client between them.
    pub fn with_lastfm(mut self, lastfm: Arc<LastFm>) -> Self {
        self.insert("lastfm_recent_plays", Arc::new(RecentPlays(Arc::clone(&lastfm))));
        self.insert("lastfm_current_track", Arc::new(CurrentTrack(Arc::clone(&lastfm))));
        self.insert("lastfm_scrobbles_db", Arc::new(ScrobblesDb(Arc::clone(&lastfm))));
        self.insert("lastfm_top_tracks_gist", Arc::new(TopTracksGist(lastfm)));
        self
    }

    /// Registers one built-in under `name`.
    fn insert(&mut self, name: &str, job: Arc<dyn Job>) {
        self.entries.insert(name.to_string(), job);
    }

    /// Looks up a built-in, failing with the list of what is available.
    ///
    /// The available list matters: the usual cause of a miss is a Last.fm job
    /// referenced without Last.fm credentials in the environment, and an empty
    /// list is the clue that points at it.
    pub fn resolve(&self, name: &str) -> Result<Arc<dyn Job>> {
        match self.entries.get(name) {
            Some(job) => Ok(Arc::clone(job)),
            None => bail!(
                "Unknown builtin '{name}'. Registered builtins: {}",
                self.names()
            ),
        }
    }

    /// Comma-separated list of registered names, for error messages.
    fn names(&self) -> String {
        if self.entries.is_empty() {
            return "(none)".to_string();
        }
        self.entries.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_registry_says_so() {
        let Err(err) = Registry::new().resolve("lastfm_recent_plays") else {
            panic!("an empty registry should not resolve anything");
        };

        assert!(err.to_string().contains("Unknown builtin"));
        assert!(err.to_string().contains("(none)"));
    }
}
