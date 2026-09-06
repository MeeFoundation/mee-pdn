//! The hosted-identities record: per hosted identity its `PdnId` and its
//! directory namespace, nothing else — a second record of what the
//! directory holds could only disagree with it. Replaced whole on every
//! change (written beside, renamed over), so an unreadable file means
//! corruption rather than a kill caught mid-write, and stops the start.

use std::path::Path;

use anyhow::{Context as _, Result};
use data_layer::NamespaceId;
use pdn_types::PdnId;
use serde::{Deserialize, Serialize};

pub(crate) const HOSTED_IDENTITIES_FILE: &str = "hosted-identities.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostedLine {
    pub(crate) identity: PdnId,
    pub(crate) directory: NamespaceId,
}

/// Hex strings, so the file reads like every other surface prints.
#[derive(Serialize, Deserialize)]
struct RawLine {
    identity: String,
    directory: String,
}

/// `Ok(empty)` when the file is absent — a first start.
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

/// Replace the record whole; a failure leaves the previous record intact.
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
        // A rename can commit before the data reaches the disk.
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
