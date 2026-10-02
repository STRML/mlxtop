// SPDX-License-Identifier: MIT
//! Operating-system inputs read by the collectors.
//!
//! Collectors reach commands and files only through [`Host`], so the parsing
//! and classification paths for both platforms can be driven by recorded
//! fixtures in tests. [`System`] is the production implementation; it adds no
//! behavior beyond the bounded command runner and plain file reads.
use std::fs;
use std::path::{Path, PathBuf};

use crate::command_text;

pub(crate) trait Host: Send {
    /// Standard output of a successful command, or `None`.
    fn command(&self, program: &str, args: &[&str]) -> Option<String>;
    /// Whole text file, or `None` when it cannot be read.
    fn read_file(&self, path: &Path) -> Option<String>;
    /// Entries of a directory; empty when it cannot be listed.
    fn read_dir(&self, path: &Path) -> Vec<PathBuf>;

    fn command_u64(&self, program: &str, args: &[&str]) -> Option<u64> {
        self.command(program, args)?.trim().parse().ok()
    }
}

pub(crate) struct System;

impl Host for System {
    fn command(&self, program: &str, args: &[&str]) -> Option<String> {
        command_text(program, args)
    }

    fn read_file(&self, path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }

    fn read_dir(&self, path: &Path) -> Vec<PathBuf> {
        fs::read_dir(path)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    }
}

/// Which collector family samples the host. Chosen once from the build target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    MacOs,
    Linux,
}

impl Platform {
    pub(crate) fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

#[cfg(test)]
#[path = "tests/host.rs"]
mod tests;
