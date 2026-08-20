//! The hosted-identities record: one file in the runtime's storage
//! directory naming, per hosted identity, its `PdnId` and the namespace of
//! its private metadata directory — and nothing else. The directory is the
//! durable record of an identity's own state (Invariant 1), so everything
//! else re-derives from it at recovery; a second record of the same facts
//! could disagree with the stores, and the disagreement would be discovered
//! by a decision acting on it.
//!
//! Every change replaces the file whole — written beside, renamed over —
//! never editing it in place: an interrupted change leaves the previous
//! record intact and the operation failed. A file that cannot be read or
//! parsed therefore means corruption, not a routine kill caught mid-write,
//! and it stops the start; an absent file is a first start.

use std::path::Path;

use anyhow::{Context as _, Result};
use data_layer::NamespaceId;
use pdn_types::PdnId;
use serde::{Deserialize, Serialize};

/// The record's file name inside the runtime's storage directory.
pub(crate) const HOSTED_IDENTITIES_FILE: &str = "hosted-identities.json";

/// One hosted identity as the record names it: the identity and its private
/// metadata directory's namespace. Serialized as hex strings, so a person
/// looking at the file reads the same identifiers every other surface
/// prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedLine {
    pub(crate) identity: PdnId,
    pub(crate) directory: NamespaceId,
}

/// The serialized shape of one line — strings on purpose (see
/// [`HostedLine`]).
#[derive(Serialize, Deserialize)]
struct RawLine {
    identity: String,
    directory: String,
}

/// Read the record, `Ok(empty)` when the file is absent — a first start. A
/// file that exists but cannot be read or parsed is an error naming it: a
/// start that hosted nothing from an unreadable record would look healthy
/// while answering every request with "not hosted".
pub(crate) fn read_record(dir: &Path) -> Result<Vec<HostedLine>> {
    let path = dir.join(HOSTED_IDENTITIES_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "cannot read the hosted-identities record {}",
                    path.display()
                )
            })
        }
    };
    let raw: Vec<RawLine> = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "cannot parse the hosted-identities record {}",
            path.display()
        )
    })?;
    raw.into_iter()
        .map(|line| {
            Ok(HostedLine {
                identity: line.identity.parse().map_err(|err| {
                    anyhow::anyhow!(
                        "cannot parse an identity in the hosted-identities record {}: {err}",
                        path.display()
                    )
                })?,
                directory: line.directory.parse().map_err(|err| {
                    anyhow::anyhow!(
                        "cannot parse a namespace in the hosted-identities record {}: {err}",
                        path.display()
                    )
                })?,
            })
        })
        .collect()
}

/// Replace the record with `lines`, whole: serialized beside the file and
/// renamed over it. A failure at any point — a full disk above all — leaves
/// the previous record intact and surfaces as the failed operation, so
/// recording a second identity cannot lose the first.
pub(crate) fn write_record(dir: &Path, lines: &[HostedLine]) -> Result<()> {
    use std::io::Write as _;
    let path = dir.join(HOSTED_IDENTITIES_FILE);
    let staged = dir.join(format!("{HOSTED_IDENTITIES_FILE}.tmp"));
    let raw: Vec<RawLine> = lines
        .iter()
        .map(|line| RawLine {
            identity: line.identity.to_string(),
            directory: line.directory.to_string(),
        })
        .collect();
    let bytes = serde_json::to_vec_pretty(&raw)?;
    {
        let mut file = std::fs::File::create(&staged).with_context(|| {
            format!(
                "cannot stage the hosted-identities record beside {}",
                path.display()
            )
        })?;
        file.write_all(&bytes).with_context(|| {
            format!(
                "cannot stage the hosted-identities record beside {}",
                path.display()
            )
        })?;
        // Synced before the rename: a rename can commit before the data
        // reaches the disk, and a kill between the two would leave a
        // truncated record where the previous one stood.
        file.sync_all().with_context(|| {
            format!(
                "cannot stage the hosted-identities record beside {}",
                path.display()
            )
        })?;
    }
    std::fs::rename(&staged, &path).with_context(|| {
        format!(
            "cannot replace the hosted-identities record {}",
            path.display()
        )
    })
}
