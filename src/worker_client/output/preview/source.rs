use super::Gap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const LOCATION_BYTES: usize = super::TEXT_BYTES / 16;

/// Keep the generated session/file suffix exact when the configured recording
/// prefix is too long. That suffix is bounded independently of the host path.
fn recording_location(path: &Path, session: &Path, directory: bool) -> String {
    let separator = if directory {
        std::path::MAIN_SEPARATOR_STR
    } else {
        ""
    };
    let full = format!("{}{separator}", path.display());
    if full.len() <= LOCATION_BYTES {
        return full;
    }
    let console = session
        .parent()
        .expect("recording sessions directory")
        .parent()
        .expect("Console recording directory");
    format!(
        "{}{separator} (relative to Console recording directory)",
        path.strip_prefix(console)
            .expect("session-owned recording location")
            .display()
    )
}

#[derive(Clone, Default)]
pub(in crate::worker_client::output) struct Source {
    pub(in crate::worker_client::output) owner: Option<crate::transcript::Owner>,
    pub(in crate::worker_client::output) file: Option<crate::transcript::OutputRecord>,
    pub(in crate::worker_client::output) journal: Option<Arc<Mutex<PathBuf>>>,
    pub(in crate::worker_client::output) raw_bytes: u64,
    pub(in crate::worker_client::output) retained_bytes: u64,
    pub(in crate::worker_client::output) discarded_bytes: u64,
}

/// A single file, or its directory when several retained files contribute.
/// All receipts in a Console response belong to the same recording.
#[derive(Clone, Default)]
pub(super) struct Location {
    path: Option<String>,
    directory: bool,
    public_directory: Option<Arc<Mutex<PathBuf>>>,
}

impl Location {
    pub(super) fn extend(&mut self, other: Self) {
        if self.public_directory.is_none() {
            self.public_directory = other.public_directory.clone();
        }
        if let Some(path) = other.path {
            self.add(&path, other.directory);
        }
    }

    pub(super) fn add_artifact(&mut self, artifact: &crate::transcript::Artifact) {
        self.public_directory = Some(artifact.public_directory.clone());
        self.add(&artifact.path, false);
    }

    fn resolved_path(&self) -> Option<PathBuf> {
        let path = PathBuf::from(self.path.as_ref()?);
        Some(match &self.public_directory {
            Some(directory) => {
                let directory = directory.lock().expect("recording path lock");
                if directory.as_os_str().is_empty() {
                    return None;
                }
                directory.join(path)
            }
            None => path,
        })
    }

    pub(super) fn add(&mut self, path: &str, directory: bool) {
        match &self.path {
            None => {
                self.path = Some(path.to_owned());
                self.directory = directory;
            }
            Some(previous) if previous == path && self.directory == directory => {}
            Some(previous) => {
                let previous = Path::new(previous);
                let incoming = Path::new(path);
                let mut common = if self.directory {
                    previous
                } else {
                    previous.parent().expect("recorded file parent")
                };
                let incoming = if directory {
                    incoming
                } else {
                    incoming.parent().expect("recorded file parent")
                };
                while !incoming.starts_with(common) {
                    common = common.parent().expect("one recording owns retained paths");
                }
                self.path = Some(common.display().to_string());
                self.directory = true;
            }
        }
    }

    /// Whether this names a retained file in a recording whose directory the
    /// recording writer has not chosen yet.
    fn unpublished(&self) -> bool {
        self.path.is_some() && self.resolved_path().is_none()
    }

    pub(super) fn describe(&self, singular: &str, plural: &str) -> Option<String> {
        self.resolved_path().map(|path| {
            format!(
                "{}: {}",
                if self.directory { plural } else { singular },
                recording_location(&path, recording_root(&path), self.directory)
            )
        })
    }

    fn shared_recording(&self, images: &Self) -> Option<String> {
        let logs = self.resolved_path()?;
        let images_path = images.resolved_path()?;
        let recording = recording_root(&logs);
        Some(format!(
            "retained output: {}; logs: {}{}; images: {}{}",
            recording_location(recording, recording, true),
            logs.strip_prefix(recording)
                .expect("recording-owned logs")
                .display(),
            if self.directory {
                std::path::MAIN_SEPARATOR_STR
            } else {
                ""
            },
            images_path
                .strip_prefix(recording)
                .expect("recording-owned images")
                .display(),
            if images.directory {
                std::path::MAIN_SEPARATOR_STR
            } else {
                ""
            },
        ))
    }
}

fn recording_root(path: &Path) -> &Path {
    path.ancestors()
        .find(|p| p.file_name().is_some_and(|name| name == "capture"))
        .and_then(Path::parent)
        .expect("capture belongs to a recording")
}

/// Constant-size accounting for fully omitted response intervals. Individual
/// file paths and cumulative raw counts remain owned by their recording receipts.
#[derive(Clone, Default)]
pub(in crate::worker_client::output) struct Summary {
    pub(super) gap: Gap,
    pub(super) owner: Option<crate::transcript::Owner>,
    has_source: bool,
    logs: Location,
    // Keep the uncommon process-output location off the inline Part size;
    // every retained control and text fragment shares that enum's allocation.
    journal: Option<Box<Location>>,
    journal_partial: bool,
    partial: bool,
    unavailable: bool,
}

impl Summary {
    pub(super) fn new(source: &Source, gap: Gap) -> Self {
        if let Some(file) = &source.file {
            // Publish the retired interval before releasing its receipt;
            // active polls replay unclaimed output before collecting more.
            file.note_inline_omission(gap.bytes);
            file.publish();
        }
        let mut summary = Self {
            gap,
            ..Self::default()
        };
        summary.source(source);
        summary
    }

    pub(super) fn source(&mut self, source: &Source) {
        if !self.has_source {
            self.owner = source.owner;
            self.has_source = true;
        } else if self.owner != source.owner {
            self.owner = None;
        }
        if let Some(file) = &source.file {
            if source.retained_bytes != 0 {
                self.logs.public_directory = Some(file.public_directory.clone());
                self.logs.add(&file.path, false);
            }
            // The writer can truncate a file after its interval was sealed.
            self.partial |= source.discarded_bytes != 0 || file.truncated();
        } else if let Some(directory) = &source.journal {
            if source.retained_bytes != 0 {
                let journal = self.journal.get_or_insert_default();
                journal.public_directory = Some(directory.clone());
                journal.add("capture/events.jsonl", false);
            }
            self.journal_partial |= source.discarded_bytes != 0;
        } else {
            self.unavailable |= source.raw_bytes != 0;
        }
    }

    pub(super) fn extend(&mut self, other: Self) {
        if other.has_source {
            if !self.has_source {
                self.owner = other.owner;
                self.has_source = true;
            } else if self.owner != other.owner {
                self.owner = None;
            }
        }
        self.gap.bytes += other.gap.bytes;
        self.gap.notices += other.gap.notices;
        self.partial |= other.partial;
        self.unavailable |= other.unavailable;
        self.logs.extend(other.logs);
        if let Some(journal) = other.journal {
            self.journal.get_or_insert_default().extend(*journal);
        }
        self.journal_partial |= other.journal_partial;
    }

    /// Whether this omission's marker names a retained file once its
    /// recording is published: a raw log, a capture journal, or an image.
    pub(super) fn cites(&self, artifacts: &Location) -> bool {
        self.logs.path.is_some() || self.journal.is_some() || artifacts.path.is_some()
    }

    pub(super) fn notice(
        &self,
        images: u64,
        image_bytes: u64,
        recorded_images: u64,
        artifacts: &Location,
        writer: crate::transcript::WriterState,
    ) -> String {
        let mut details = Vec::new();
        if self.gap.bytes != 0 {
            details.push(format!("{} UTF-8 bytes", self.gap.bytes));
        }
        if images != 0 {
            details.push(format!(
                "{images} {} ({image_bytes} encoded bytes)",
                if images == 1 { "image" } else { "images" }
            ));
        }
        let omitted = details.join(", ");
        details.clear();
        // Until the writer publishes the recording, its receipts have no path
        // to give. Their output is queued, not lost, unless recording stopped.
        let recording = writer != crate::transcript::WriterState::Stopped;
        let pending_text = recording
            && (self.logs.unpublished()
                || self.journal.as_deref().is_some_and(Location::unpublished));
        let pending_images = recording && artifacts.unpublished();
        let shared = self.logs.shared_recording(artifacts);
        let shared_location = shared.is_some();
        // Whether this marker names a retained file.
        let mut cited = shared_location;
        if let Some(location) = shared {
            details.push(location);
        }
        if self.gap.bytes != 0 {
            let journal = self
                .journal
                .as_ref()
                .and_then(|journal| journal.describe("capture journal", "capture journals"));
            if !shared_location {
                if let Some(logs) = self.logs.describe("raw log", "retained logs") {
                    details.push(logs);
                    cited = true;
                } else if journal.is_none() && !pending_text {
                    details.push("no retained log; omitted text unavailable".to_owned());
                }
            }
            if let Some(journal) = journal {
                details.push(journal);
                cited = true;
                if self.journal_partial {
                    details.push(
                        "journal contains prefixes; later process text unavailable".to_owned(),
                    );
                }
            }
            if self.logs.path.is_some() || self.journal.is_some() {
                if self.partial {
                    details.push("logs contain prefixes; later raw text unavailable".to_owned());
                }
                if self.unavailable {
                    details.push("some omitted text unavailable".to_owned());
                }
            }
            if self.gap.notices != 0 {
                details.push(format!(
                    "{} generated notice bytes unavailable",
                    self.gap.notices
                ));
            }
        }
        if !shared_location
            && let Some(location) = artifacts.describe("retained image", "retained images")
        {
            details.push(location);
            cited = true;
        }
        let unretained_images = images
            - if artifacts.resolved_path().is_some() || pending_images {
                recorded_images
            } else {
                0
            };
        if unretained_images != 0 {
            details.push(format!(
                "{unretained_images} {} not retained",
                if unretained_images == 1 {
                    "image"
                } else {
                    "images"
                }
            ));
        }
        // Receipts are issued when output is queued for the recording writer.
        // Qualify the files this marker names when the writer has not caught
        // up, or when recording stopped and discarded what was still queued.
        let qualifier = match writer {
            crate::transcript::WriterState::Settled => None,
            crate::transcript::WriterState::Behind => Some("recording writer stalled"),
            crate::transcript::WriterState::Stopped => Some("recording stopped"),
        };
        if pending_text || pending_images {
            // Only a reply's bounded wait finds the writer stalled.
            details.push(
                if writer == crate::transcript::WriterState::Behind {
                    "recording writer stalled; retained files are not yet published"
                } else {
                    "retained files are not yet published"
                }
                .to_owned(),
            );
        } else if let Some(qualifier) = qualifier
            && cited
        {
            details.push(format!(
                "{qualifier}; retained files may be missing or incomplete"
            ));
        }
        format!("\n[output omitted: {omitted}; {}]\n", details.join("; "))
    }
}
