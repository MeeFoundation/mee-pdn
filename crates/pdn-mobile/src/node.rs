//! The handle an application holds: one handle, one node.
//!
//! Bring-up and stop are explicit acts. A handle refuses a second bring-up
//! of its own rather than replacing its node, and a stop is safe to repeat.
//! The constraint is on the handle rather than on the process, so a test
//! binary holding 2 handles against 2 runtimes stays possible while an
//! application cannot grow a node set behind one — an arrangement that
//! could not show the act it would exist for, since a device that goes away
//! takes every runtime co-located with it.
//!
//! The facade owns the asynchronous runtime the operations need: a foreign
//! caller brings none, and every exported call is awaited rather than
//! blocking a caller's thread until the network answers. Where a caller's
//! continuation resumes is the caller's own affair.
//!
//! Storage is named by the embedder at bring-up, because the embedder is
//! the one that knows the sandbox it may write in. Nothing here derives a
//! directory or reads one from the environment.

use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use pdn_node::{
    ConnectionsService as _, DataService as _, IdentityService as _, InvitePayload, LinkingPayload,
    Runtime, SpawnOptions, StorageConfig, SyncService as _,
};

use crate::{
    error::{self, PdnError},
    payload,
    shapes::{self, EntryListing, GrantCapability, GrantedPath},
};

/// What a handle holds. `ComingUp` is a state of its own so 2 concurrent
/// bring-ups cannot both reach the spawn.
enum State {
    Down,
    ComingUp,
    Up(Arc<Runtime>),
}

/// One node, reached through one handle.
///
/// Every call names the identity or the issuer it acts for, and the runtime
/// keeps those apart; the handle holds no notion of a current identity, so
/// nothing here can act as one identity while a screen believes it is
/// acting as another.
#[derive(uniffi::Object)]
pub struct PdnNode {
    /// The asynchronous runtime the facade owns. Taken on drop, so the
    /// teardown of a handle released without a stop runs on a thread of its
    /// own rather than on the thread that let it go.
    tokio: Mutex<Option<tokio::runtime::Runtime>>,
    /// Shared with the bring-up's own task, which is what installs a node:
    /// a caller whose call is cancelled — a cancelled coroutine, an
    /// abandoned task — cannot leave the state mid-transition, because the
    /// transition does not belong to the caller's future.
    state: Arc<Mutex<State>>,
}

#[uniffi::export]
impl PdnNode {
    /// A handle with no node behind it yet.
    #[uniffi::constructor]
    pub fn new() -> Result<Arc<Self>, PdnError> {
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| {
                tracing::error!("the facade's asynchronous runtime did not start: {err}");
                PdnError::Internal
            })?;
        Ok(Arc::new(Self {
            tokio: Mutex::new(Some(tokio)),
            state: Arc::new(Mutex::new(State::Down)),
        }))
    }

    /// Bring the node up on `storage_dir`, reconciling every
    /// `reconcile_interval_secs`.
    ///
    /// The directory is the node's only copy of what it holds: its
    /// replicas, its payloads and its own key live there, and a node
    /// brought up on it again is the same node. A directory belongs to one
    /// running node, so a bring-up on one another node holds is refused as
    /// that rather than as a corrupt store.
    ///
    /// The interval is named rather than defaulted, because a short one
    /// costs radio wakeups and battery and the choice belongs to a host
    /// that knows what it is running. It bounds a published grant reaching
    /// the peer, a write reaching another device of the same identity, and
    /// a linking catch-up — not the grantee's read of a granted claim,
    /// which nudges its own reconciliation.
    ///
    /// Installing the node is the spawn's own last act rather than this
    /// call's, so a caller that abandons the call — a cancelled coroutine,
    /// a task that goes away — leaves the handle either up or down and
    /// never mid-transition. A stop that overtakes a bring-up wins: the
    /// node that had just come up is shut down again, and the bring-up
    /// reports the node as not up rather than handing back one the caller
    /// has already asked to stop.
    pub async fn bring_up(
        &self,
        storage_dir: String,
        reconcile_interval_secs: u64,
    ) -> Result<(), PdnError> {
        let interval = shapes::duration(reconcile_interval_secs, "reconcile interval")?;
        let options = SpawnOptions {
            storage: StorageConfig::Directory(std::path::PathBuf::from(storage_dir)),
            reconcile_interval: interval,
        };
        self.claim_bring_up()?;
        let state = Arc::clone(&self.state);
        let installed = self
            .on_node(async move { install(&state, Runtime::spawn(options).await).await })
            .await?;
        if installed {
            Ok(())
        } else {
            Err(PdnError::NodeNotUp)
        }
    }

    /// Stop the node. Safe to repeat: a stop with nothing up is a no-op
    /// rather than a failure.
    ///
    /// What a stop costs is availability, not what the node holds. A peer
    /// that needed data while the node was down did not get it, and the
    /// identities, connections and entries are found again on the next
    /// bring-up. What does not come back is work in flight — an invite
    /// minted and not consumed, a ceremony interrupted.
    pub async fn stop(&self) -> Result<(), PdnError> {
        let runtime = {
            let mut state = self.state()?;
            match std::mem::replace(&mut *state, State::Down) {
                State::Up(runtime) => Some(runtime),
                State::Down | State::ComingUp => None,
            }
        };
        match runtime {
            None => Ok(()),
            Some(runtime) => self.on_node(async move { runtime.shutdown().await }).await,
        }
    }

    /// The node id this node answers as — the same one after a bring-up on
    /// the same directory, because the key that decides it lives there.
    pub async fn node_id(&self) -> Result<String, PdnError> {
        let runtime = self.runtime()?;
        self.on_node(async move { Ok(runtime.sync().node_id().to_string()) })
            .await
    }

    /// Create an identity on this node — its first device.
    ///
    /// An identity is a placeholder value with no key material behind it,
    /// so nothing binds one to a person. A connection is evidence that 2
    /// devices ran a ceremony with the same one-time secret, and nothing
    /// more.
    pub async fn create_identity(&self) -> Result<String, PdnError> {
        let runtime = self.runtime()?;
        self.on_node(async move { Ok(runtime.identity().create().await?.to_string()) })
            .await
    }

    /// The identities this node hosts.
    pub async fn hosted_identities(&self) -> Result<Vec<String>, PdnError> {
        let runtime = self.runtime()?;
        self.on_node(async move {
            let hosted = runtime.sync().hosted_identities().await?;
            Ok(hosted.iter().map(ToString::to_string).collect())
        })
        .await
    }

    /// Mint a payload for another device to join `identity`.
    ///
    /// The payload carries a live one-time secret: nothing in it grants
    /// durable access, which is why it is safe to show to a camera and
    /// unsafe to leave on a table. Whoever photographs it can consume it in
    /// the intended device's place until the secret burns or expires.
    pub async fn mint_linking_payload(
        &self,
        identity: String,
        lifetime_secs: Option<u64>,
    ) -> Result<String, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let lifetime = shapes::lifetime(lifetime_secs)?;
        let runtime = self.runtime()?;
        let minted = self
            .on_node(async move { runtime.identity().linking_invite(identity, lifetime).await })
            .await?;
        payload::encode(&minted)
    }

    /// Join the identity a linking payload names, as a device of it.
    ///
    /// `budget_secs` bounds the whole act: the dialogue spends from it
    /// first and the catch-up takes what remains. A catch-up that does not
    /// finish inside it reports its own failure and leaves nothing behind.
    pub async fn consume_linking_payload(
        &self,
        code: String,
        budget_secs: u64,
    ) -> Result<(), PdnError> {
        let payload: LinkingPayload = payload::decode(&code)?;
        let budget = shapes::duration(budget_secs, "budget")?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.identity().link(payload, budget).await })
            .await
    }

    /// Mint an invitation for another identity to connect to `identity`.
    /// The exposure is the one [`Self::mint_linking_payload`] describes.
    pub async fn mint_invite(
        &self,
        identity: String,
        lifetime_secs: Option<u64>,
    ) -> Result<String, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let lifetime = shapes::lifetime(lifetime_secs)?;
        let runtime = self.runtime()?;
        let minted = self
            .on_node(async move { runtime.connections().invite(identity, lifetime).await })
            .await?;
        payload::encode(&minted)
    }

    /// Establish a connection for `identity` from an invitation.
    ///
    /// A code read for the wrong act reaches the runtime and comes back as
    /// the runtime's refusal: nothing above the facade parses a payload to
    /// tell an invitation from a device joining an identity.
    pub async fn consume_invite(&self, identity: String, code: String) -> Result<(), PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let payload: InvitePayload = payload::decode(&code)?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.connections().establish(identity, payload).await })
            .await
    }

    /// The current connections of `identity`.
    pub async fn connections(&self, identity: String) -> Result<Vec<String>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            let peers = runtime.connections().list(identity).await?;
            Ok(peers.iter().map(ToString::to_string).collect())
        })
        .await
    }

    /// Publish a grant: `identity` grants `peer` read — and, per claim,
    /// write — on exactly these paths of `issuer`'s data.
    ///
    /// The issuer must be the granting identity itself; anything else is
    /// the runtime's refusal. One grant record exists per granted issuer
    /// toward a peer, so publishing again replaces the previous record.
    pub async fn publish_grant(
        &self,
        identity: String,
        peer: String,
        issuer: String,
        claims: Vec<GrantedPath>,
    ) -> Result<(), PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let peer = shapes::identity(&peer, "peer")?;
        let issuer = shapes::identity(&issuer, "issuer")?;
        let claims = shapes::granted_claims(issuer, &claims)?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            runtime
                .connections()
                .publish_grant(identity, peer, issuer, claims)
                .await
        })
        .await
    }

    /// What `peer` granted `identity` — the capability alone, with the
    /// replica's ticket dropped here and reaching nothing above.
    ///
    /// The read reports what is readable now and never waits, so a grant
    /// that has not replicated in reads as no grant. An empty answer is
    /// therefore not evidence that the peer granted nothing.
    pub async fn read_peer_grants(
        &self,
        identity: String,
        peer: String,
    ) -> Result<Vec<GrantCapability>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let peer = shapes::identity(&peer, "peer")?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            let grants = runtime.connections().read_grants(identity, peer).await?;
            Ok(grants
                .into_iter()
                .map(|held| GrantCapability::from(held.grant))
                .collect())
        })
        .await
    }

    /// What `identity` granted `peer`, read from this node rather than
    /// remembered.
    ///
    /// The answer is this device's: it says the record is here, never that
    /// it reached a sibling device or the peer. An empty answer covers no
    /// connection, a pair whose tickets have not replicated here, and
    /// nothing granted.
    pub async fn read_own_grants(
        &self,
        identity: String,
        peer: String,
    ) -> Result<Option<GrantCapability>, PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let peer = shapes::identity(&peer, "peer")?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            Ok(runtime
                .connections()
                .read_own_grants(identity, peer)
                .await?
                .map(GrantCapability::from))
        })
        .await
    }

    /// Withdraw the grant of `issuer`'s data toward `peer` — one act.
    ///
    /// The grantee's node unbinds the namespace once the tombstone
    /// replicates, so its later reads of that issuer are refused rather
    /// than answered empty. What was already delivered is not recalled:
    /// nothing compels a node that received data to forget it.
    pub async fn withdraw_grant(
        &self,
        identity: String,
        peer: String,
        issuer: String,
    ) -> Result<(), PdnError> {
        let identity = shapes::identity(&identity, "identity")?;
        let peer = shapes::identity(&peer, "peer")?;
        let issuer = shapes::identity(&issuer, "issuer")?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            runtime
                .connections()
                .withdraw_grant(identity, peer, issuer)
                .await
        })
        .await
    }

    /// Write `payload` at `path` of `issuer`'s data.
    ///
    /// A write into a namespace received under a grant is admitted locally
    /// against the grant record this node has read; the issuer's own gate
    /// decides afterwards, and a claim its record does not cover is
    /// retracted there with no answer here carrying that verdict.
    pub async fn write_entry(
        &self,
        issuer: String,
        path: String,
        payload: Vec<u8>,
    ) -> Result<(), PdnError> {
        let issuer = shapes::identity(&issuer, "issuer")?;
        let path = shapes::entry_path(&path)?;
        shapes::entry_payload(&payload)?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.data().write(issuer, &path, &payload).await })
            .await
    }

    /// Read the entry at `path` of `issuer`'s data.
    ///
    /// No value covers 2 situations the runtime does not distinguish: no
    /// entry exists, and an entry's record has arrived while its payload
    /// has not. Records and payloads travel independently, so an empty read
    /// is not proof of absence and a caller reads again rather than
    /// concluding.
    pub async fn read_entry(
        &self,
        issuer: String,
        path: String,
    ) -> Result<Option<Vec<u8>>, PdnError> {
        let issuer = shapes::identity(&issuer, "issuer")?;
        let path = shapes::entry_path(&path)?;
        let runtime = self.runtime()?;
        self.on_node(async move { runtime.data().read(issuer, &path).await })
            .await
    }

    /// List the entries of `issuer`'s data, with no payloads fetched. A
    /// prefix restricts the listing, matching whole path components rather
    /// than characters.
    pub async fn list_entries(
        &self,
        issuer: String,
        path_prefix: Option<String>,
    ) -> Result<Vec<EntryListing>, PdnError> {
        let issuer = shapes::identity(&issuer, "issuer")?;
        let prefix = path_prefix.as_deref().map(shapes::entry_path).transpose()?;
        let runtime = self.runtime()?;
        self.on_node(async move {
            let entries = runtime.data().list(issuer, prefix.as_ref()).await?;
            Ok(entries.into_iter().map(EntryListing::from).collect())
        })
        .await
    }
}

impl PdnNode {
    /// The state lock. A poisoned lock means a panic ran while it was held,
    /// which is a defect rather than a refusal.
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, PdnError> {
        self.state.lock().map_err(|_| {
            tracing::error!("the facade's state lock is poisoned");
            PdnError::Internal
        })
    }

    /// Claim the transition into a bring-up, refusing a second one.
    fn claim_bring_up(&self) -> Result<(), PdnError> {
        let mut state = self.state()?;
        match *state {
            State::Down => {
                *state = State::ComingUp;
                Ok(())
            }
            State::ComingUp | State::Up(_) => Err(PdnError::NodeAlreadyUp),
        }
    }

    /// The node an exported call acts on.
    fn runtime(&self) -> Result<Arc<Runtime>, PdnError> {
        match &*self.state()? {
            State::Up(runtime) => Ok(Arc::clone(runtime)),
            State::Down | State::ComingUp => Err(PdnError::NodeNotUp),
        }
    }

    /// Run one operation on the facade's own asynchronous runtime and map
    /// what it reports through the table. No lock is held across the await.
    async fn on_node<T>(
        &self,
        act: impl Future<Output = anyhow::Result<T>> + Send + 'static,
    ) -> Result<T, PdnError>
    where
        T: Send + 'static,
    {
        let joined = {
            let tokio = self.tokio.lock().map_err(|_| {
                tracing::error!("the facade's runtime lock is poisoned");
                PdnError::Internal
            })?;
            match tokio.as_ref() {
                Some(tokio) => tokio.spawn(act),
                // Only reachable while the handle is being released.
                None => return Err(PdnError::Internal),
            }
        };
        match joined.await {
            Ok(outcome) => outcome.map_err(|err| error::table(&err)),
            Err(err) => {
                tracing::error!("an exported call did not finish: {err}");
                Err(PdnError::Internal)
            }
        }
    }
}

/// The bring-up's last act, inside the bring-up's own task: put the node
/// where the exported calls look for it, and report whether it went there.
///
/// It does not go there if a stop overtook the bring-up, and then this is
/// what shuts the node down again — otherwise a node nobody holds would
/// keep its endpoint bound and its directory held, and the next bring-up
/// would be refused by a node the caller had already stopped.
async fn install(state: &Mutex<State>, spawned: anyhow::Result<Runtime>) -> anyhow::Result<bool> {
    let runtime = match spawned {
        Ok(runtime) => runtime,
        Err(err) => {
            if let Ok(mut guard) = state.lock() {
                *guard = State::Down;
            }
            return Err(err);
        }
    };
    // Held until the lock says which of the 2 fates is this node's.
    let mut held = Some(runtime);
    let installed = {
        let mut guard = state
            .lock()
            .map_err(|_| anyhow::anyhow!("the facade's state lock is poisoned"))?;
        match *guard {
            State::ComingUp => {
                if let Some(runtime) = held.take() {
                    *guard = State::Up(Arc::new(runtime));
                }
                true
            }
            State::Down | State::Up(_) => false,
        }
    };
    if let Some(runtime) = held {
        runtime.shutdown().await?;
    }
    Ok(installed)
}

impl Drop for PdnNode {
    fn drop(&mut self) {
        // A handle released without a stop must not block the thread that
        // let it go, and the node's own teardown does block: the store's
        // handle shuts its actor down and joins its thread as it is
        // dropped. So the runtime and whatever it still holds move to a
        // thread of their own, which stops the node properly and then lets
        // the runtime go.
        let tokio = self.tokio.lock().ok().and_then(|mut held| held.take());
        let node = self
            .state
            .lock()
            .ok()
            .map(|mut guard| std::mem::replace(&mut *guard, State::Down));
        let Some(tokio) = tokio else { return };
        let teardown = std::thread::Builder::new()
            .name("pdn-mobile-teardown".to_owned())
            .spawn(move || {
                if let Some(State::Up(runtime)) = node {
                    tokio.block_on(async move {
                        if let Err(err) = runtime.shutdown().await {
                            tracing::error!("a released handle's node did not stop cleanly: {err}");
                        }
                    });
                }
                drop(tokio);
            });
        if let Err(err) = teardown {
            tracing::error!("a released handle's teardown thread did not start: {err}");
        }
    }
}
