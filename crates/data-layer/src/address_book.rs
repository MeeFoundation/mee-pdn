//! The node's address book: the last addresses its endpoint knew for every
//! peer a hosted replica syncs with or names as a contact, kept in the
//! storage directory and handed back to the endpoint at the next spawn.
//! Spec: data-layer `durable-storage`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use iroh::{address_lookup::MemoryLookup, Endpoint, EndpointAddr, EndpointId, TransportAddr};

const ADDRESS_BOOK_FILE: &str = "peers";

/// How far the file lags behind the endpoint while the node runs.
pub(crate) const ADDRESS_BOOK_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(crate) struct AddressBook {
    directory: PathBuf,
    lookup: MemoryLookup,
    entries: Mutex<BTreeMap<EndpointId, EndpointAddr>>,
    /// The file did not read back: the next refresh writes it, changed or
    /// not, so a bad file does not outlive one run.
    rewrite: AtomicBool,
    /// Held through a refresh, its write included: the pass's refresh and
    /// shutdown's can overlap, and two writers on one staging file commit a
    /// mix of both.
    refreshing: tokio::sync::Mutex<()>,
}

impl AddressBook {
    /// The book of the node on `directory`, seeded from its file. An
    /// unreadable file starts the book empty: the addresses are a hint, and
    /// a hint lost costs a dial, not the start.
    pub(crate) fn open(directory: &Path) -> Self {
        let path = directory.join(ADDRESS_BOOK_FILE);
        let read = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<Vec<EndpointAddr>>(&bytes).map_err(anyhow::Error::from)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err.into()),
        };
        let (addrs, rewrite) = match read {
            Ok(addrs) => (addrs, false),
            Err(err) => {
                tracing::warn!(path = %path.display(), "the address book is unreadable, starting without it: {err:#}");
                (Vec::new(), true)
            }
        };
        let entries: BTreeMap<EndpointId, EndpointAddr> =
            addrs.into_iter().map(|addr| (addr.id, addr)).collect();
        let lookup = MemoryLookup::from_endpoint_info(entries.values().cloned());
        Self {
            directory: directory.to_path_buf(),
            lookup,
            entries: Mutex::new(entries),
            rewrite: AtomicBool::new(rewrite),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// What the endpoint resolves a node id with when nothing else names
    /// its addresses.
    pub(crate) fn lookup(&self) -> MemoryLookup {
        self.lookup.clone()
    }

    /// Take what `endpoint` knows of `peers` now, and rewrite the file when
    /// the book changed.
    pub(crate) async fn refresh(
        &self,
        endpoint: &Endpoint,
        peers: BTreeSet<EndpointId>,
    ) -> Result<()> {
        let _refreshing = self.refreshing.lock().await;
        let mut known = BTreeMap::new();
        for peer in &peers {
            if let Some(info) = endpoint.remote_info(*peer).await {
                let addrs: Vec<TransportAddr> = info.into_addrs().map(Into::into).collect();
                if !addrs.is_empty() {
                    known.insert(*peer, EndpointAddr::from_parts(*peer, addrs));
                }
            }
        }
        let next = {
            let mut entries = self
                .entries
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("address book lock poisoned"))?;
            let next = merged(&entries, &peers, known);
            if next == *entries && !self.rewrite.load(Ordering::Acquire) {
                return Ok(());
            }
            for gone in entries.keys().filter(|id| !next.contains_key(*id)) {
                let _dropped = self.lookup.remove_endpoint_info(*gone);
            }
            for addr in next.values() {
                let _previous = self.lookup.set_endpoint_info(addr.clone());
            }
            entries.clone_from(&next);
            next
        };
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || write(&directory, &next))
            .await
            .context("the address book write did not finish")??;
        self.rewrite.store(false, Ordering::Release);
        Ok(())
    }
}

/// The book after a refresh: every peer still named, with what the endpoint
/// knows of it now, or else with what the book held.
fn merged(
    held: &BTreeMap<EndpointId, EndpointAddr>,
    peers: &BTreeSet<EndpointId>,
    mut known: BTreeMap<EndpointId, EndpointAddr>,
) -> BTreeMap<EndpointId, EndpointAddr> {
    peers
        .iter()
        .filter_map(|peer| {
            known
                .remove(peer)
                .or_else(|| held.get(peer).cloned())
                .map(|addr| (*peer, addr))
        })
        .collect()
}

/// Written beside and renamed into place, so a reader never meets half a
/// book.
fn write(directory: &Path, entries: &BTreeMap<EndpointId, EndpointAddr>) -> Result<()> {
    use std::io::Write;
    let path = directory.join(ADDRESS_BOOK_FILE);
    let staged = directory.join(format!("{ADDRESS_BOOK_FILE}.tmp"));
    let addrs: Vec<&EndpointAddr> = entries.values().collect();
    let encoded = serde_json::to_vec(&addrs)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&staged)
        .with_context(|| format!("cannot stage the address book beside {}", path.display()))?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    std::fs::rename(&staged, &path)
        .with_context(|| format!("cannot commit the address book {}", path.display()))
}

#[cfg(test)]
mod tests {
    use iroh::SecretKey;

    use super::*;

    fn addr(peer: EndpointId, port: u16) -> EndpointAddr {
        EndpointAddr::from_parts(
            peer,
            [TransportAddr::Ip(std::net::SocketAddr::from((
                [10, 0, 0, 7],
                port,
            )))],
        )
    }

    /// A refresh keeps the last address of a peer the endpoint has
    /// forgotten, takes the new one of a peer it knows, and drops a peer no
    /// replica names any more.
    #[test]
    fn a_refresh_keeps_the_last_address_of_every_named_peer() {
        let [b1, c1, d1] = [1u8, 2, 3].map(|seed| SecretKey::from_bytes(&[seed; 32]).public());
        let held = BTreeMap::from([
            (b1, addr(b1, 4001)),
            (c1, addr(c1, 4002)),
            (d1, addr(d1, 4003)),
        ]);
        let peers = BTreeSet::from([b1, c1]);
        let known = BTreeMap::from([(c1, addr(c1, 5002))]);
        assert_eq!(
            merged(&held, &peers, known),
            BTreeMap::from([(b1, addr(b1, 4001)), (c1, addr(c1, 5002))])
        );
    }

    /// The book written reads back as it was; a file that does not parse
    /// opens an empty book and is replaced by the next write.
    #[test]
    fn a_written_book_reads_back_and_an_unreadable_one_opens_empty() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let b1 = SecretKey::from_bytes(&[1; 32]).public();
        let entries = BTreeMap::from([(b1, addr(b1, 4001))]);
        write(dir.path(), &entries)?;
        let book = AddressBook::open(dir.path());
        assert_eq!(held(&book)?, entries);
        assert_eq!(
            book.lookup
                .get_endpoint_info(b1)
                .map(|info| info.endpoint_id),
            Some(b1)
        );

        std::fs::write(dir.path().join(ADDRESS_BOOK_FILE), b"not a book")?;
        assert!(held(&AddressBook::open(dir.path()))?.is_empty());
        write(dir.path(), &entries)?;
        assert_eq!(held(&AddressBook::open(dir.path()))?, entries);
        Ok(())
    }

    fn held(book: &AddressBook) -> Result<BTreeMap<EndpointId, EndpointAddr>> {
        book.entries
            .lock()
            .map(|entries| entries.clone())
            .map_err(|_poisoned| anyhow::anyhow!("address book lock poisoned"))
    }
}
