//! The facade's closed error table: one kind per outcome a person holding
//! a phone would act differently on.
//!
//! The table is written out rather than inherited from `pdn-node-http`,
//! whose own table folds an unreachable counterparty and every ceremony
//! timeout into its unrecognized failure. That host serves a container
//! test, where the distinction a denial rests on is a refusal against a
//! defect. A person needs three different acts out of those three outcomes
//! — move closer or check the network, mint a fresh code, try again — and
//! needs a code minted by an older build reported as a version their
//! counterparty cannot speak rather than as a broken phone.
//!
//! Two limits the table respects. Where the runtime does not separate two
//! outcomes the table follows the runtime rather than promise a distinction
//! that does not exist. And the kinds the runtime has no error for at all
//! — an empty payload, a malformed path, a grant naming no claim, a call
//! before bring-up — are the host's own and are named as such
//! ([`PdnError::MalformedInput`], [`PdnError::NodeNotUp`]).
//!
//! An unrecognized failure keeps its cause chain in the platform log and
//! never in the error a screen reads, and it is never reported as a
//! refusal: laundering a defect into an access decision would make every
//! paired denial vacuous.

use pdn_node::{
    CatchUpTimeout, DelegationUnsupported, DialogueTimeout, DirectoryHeld, EstablishmentInProgress,
    EstablishmentRefused, EstablishmentTimeout, IdentityAlreadyHosted, InviterUnreachable,
    LinkingInProgress, LinkingRefused, PeerNotConnected, UnknownIdentity, UnknownIssuer,
    UnsupportedInviteVersion, UnsupportedLinkingVersion, WriteNotGranted,
};

/// Every way an exported call fails. Identities and paths travel as the
/// text a screen shows, so a refusal names what was refused rather than
/// leaving a screen to say that something went wrong.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PdnError {
    /// The local grant record covers this claim read-only.
    #[error("writing {path} of {issuer} is not granted")]
    WriteNotGranted { issuer: String, path: String },

    /// A grant named an issuer other than the granting identity.
    #[error("{identity} cannot grant the data of {issuer}")]
    DelegationUnsupported { identity: String, issuer: String },

    /// The counterparty ran the dialogue and refused it. Which of wrong,
    /// expired or already burnt applied is uniform by design.
    #[error("the counterparty refused the ceremony")]
    CeremonyRefused,

    /// This node hosts no such identity.
    #[error("this node does not host {identity}")]
    UnknownIdentity { identity: String },

    /// This node holds nothing of that issuer's data — not an empty
    /// namespace, no namespace.
    #[error("this node holds nothing of issuer {issuer}")]
    UnknownIssuer { issuer: String },

    /// The identity has no connection to that peer.
    #[error("{identity} has no connection to {peer}")]
    PeerNotConnected { identity: String, peer: String },

    /// An act of this kind is already committed or in flight for this
    /// identity: the identity is hosted already, or a ceremony of its own
    /// is running.
    #[error("an act of this kind is already committed or in flight")]
    ActAlreadyUnderway,

    /// The dial never reached the counterparty. Distinct from a refusal:
    /// the act to take is to close the distance or fix the network.
    #[error("the counterparty was never reached")]
    CounterpartyUnreachable,

    /// The counterparty was reached and the dialogue ran out of its bound.
    /// The act to take is to mint a fresh code.
    #[error("the ceremony dialogue ran out of its bound")]
    DialogueTimedOut,

    /// The dialogue succeeded and the catch-up did not finish inside the
    /// caller's bound; the attempt left nothing behind.
    #[error("the catch-up ran out of its bound")]
    CatchUpTimedOut,

    /// A scanned code was minted by a build this runtime does not speak
    /// the payload format of. The counterparty needs an update, and the
    /// phone in the person's hand is fine.
    #[error("this runtime does not speak version {version} of that payload")]
    UnsupportedPayloadVersion { version: u8 },

    /// The node's storage directory belongs to a node that is running.
    #[error("another node holds the storage directory")]
    StorageHeld,

    /// The host rejected the input before the runtime was called.
    #[error("{what}")]
    MalformedInput { what: String },

    /// An exported call arrived before a bring-up, or after a stop.
    #[error("the node is not up")]
    NodeNotUp,

    /// A bring-up arrived while this handle's node is up or coming up. One
    /// handle owns one node, and a second bring-up does not replace it.
    #[error("this handle already owns a node")]
    NodeAlreadyUp,

    /// Something this table does not name. The cause chain stays in the
    /// platform log.
    #[error("internal failure")]
    Internal,
}

impl PdnError {
    /// The host's own refusal of input the runtime has no error for.
    pub(crate) fn malformed(what: impl Into<String>) -> Self {
        Self::MalformedInput { what: what.into() }
    }
}

/// The table. Downcasting reaches through `anyhow`'s context layers, so a
/// service error wrapped on its way out still maps.
pub(crate) fn table(err: &anyhow::Error) -> PdnError {
    if let Some(refused) = err.downcast_ref::<WriteNotGranted>() {
        return PdnError::WriteNotGranted {
            issuer: refused.issuer.to_string(),
            path: refused.path.as_str().to_owned(),
        };
    }
    if let Some(refused) = err.downcast_ref::<DelegationUnsupported>() {
        return PdnError::DelegationUnsupported {
            identity: refused.identity.to_string(),
            issuer: refused.issuer.to_string(),
        };
    }
    if let Some(unknown) = err.downcast_ref::<UnknownIdentity>() {
        return PdnError::UnknownIdentity {
            identity: unknown.identity.to_string(),
        };
    }
    if let Some(unknown) = err.downcast_ref::<UnknownIssuer>() {
        return PdnError::UnknownIssuer {
            issuer: unknown.issuer.to_string(),
        };
    }
    if let Some(unconnected) = err.downcast_ref::<PeerNotConnected>() {
        return PdnError::PeerNotConnected {
            identity: unconnected.identity.to_string(),
            peer: unconnected.peer.to_string(),
        };
    }
    if let Some(unspoken) = err.downcast_ref::<UnsupportedInviteVersion>() {
        return PdnError::UnsupportedPayloadVersion {
            version: unspoken.version,
        };
    }
    if let Some(unspoken) = err.downcast_ref::<UnsupportedLinkingVersion>() {
        return PdnError::UnsupportedPayloadVersion {
            version: unspoken.version,
        };
    }
    if err.downcast_ref::<EstablishmentRefused>().is_some()
        || err.downcast_ref::<LinkingRefused>().is_some()
    {
        return PdnError::CeremonyRefused;
    }
    if err.downcast_ref::<IdentityAlreadyHosted>().is_some()
        || err.downcast_ref::<LinkingInProgress>().is_some()
        || err.downcast_ref::<EstablishmentInProgress>().is_some()
    {
        return PdnError::ActAlreadyUnderway;
    }
    if err.downcast_ref::<InviterUnreachable>().is_some() {
        return PdnError::CounterpartyUnreachable;
    }
    // The runtime reports a dial still in flight when its bound expires as
    // the dialogue's own timeout, and the table follows it.
    if err.downcast_ref::<EstablishmentTimeout>().is_some()
        || err.downcast_ref::<DialogueTimeout>().is_some()
    {
        return PdnError::DialogueTimedOut;
    }
    if err.downcast_ref::<CatchUpTimeout>().is_some() {
        return PdnError::CatchUpTimedOut;
    }
    if err.downcast_ref::<DirectoryHeld>().is_some() {
        return PdnError::StorageHeld;
    }
    tracing::error!("unmapped facade error: {err:#}");
    PdnError::Internal
}

#[cfg(test)]
mod tests {
    use anyhow::{anyhow, Context as _};
    use pdn_node::{EntryPath, PdnId};

    use super::*;

    const ISSUER: PdnId = PdnId::from_bytes([0x11; 32]);
    const PEER: PdnId = PdnId::from_bytes([0x22; 32]);

    fn kind(err: impl Into<anyhow::Error>) -> PdnError {
        table(&err.into())
    }

    /// The refusals the runtime's rules produce, each its own kind.
    #[test]
    fn refusals_keep_their_kinds() {
        let path = EntryPath::new("contact/email").unwrap();
        assert!(matches!(
            kind(WriteNotGranted {
                issuer: ISSUER,
                path,
            }),
            PdnError::WriteNotGranted { .. }
        ));
        assert!(matches!(
            kind(DelegationUnsupported {
                identity: PEER,
                issuer: ISSUER,
            }),
            PdnError::DelegationUnsupported { .. }
        ));
        assert!(matches!(
            kind(EstablishmentRefused),
            PdnError::CeremonyRefused
        ));
        assert!(matches!(kind(LinkingRefused), PdnError::CeremonyRefused));
    }

    /// What the node does not host, and what it is already busy with.
    #[test]
    fn what_the_node_does_not_host_is_its_own_kind() {
        assert!(matches!(
            kind(UnknownIdentity { identity: PEER }),
            PdnError::UnknownIdentity { .. }
        ));
        assert!(matches!(
            kind(UnknownIssuer { issuer: ISSUER }),
            PdnError::UnknownIssuer { .. }
        ));
        assert!(matches!(
            kind(PeerNotConnected {
                identity: ISSUER,
                peer: PEER,
            }),
            PdnError::PeerNotConnected { .. }
        ));
        assert!(matches!(
            kind(IdentityAlreadyHosted { identity: ISSUER }),
            PdnError::ActAlreadyUnderway
        ));
        assert!(matches!(
            kind(LinkingInProgress { identity: ISSUER }),
            PdnError::ActAlreadyUnderway
        ));
        assert!(matches!(
            kind(EstablishmentInProgress {
                identity: ISSUER,
                peer: PEER,
            }),
            PdnError::ActAlreadyUnderway
        ));
    }

    /// The three ceremony outcomes a person acts differently on, told apart
    /// without reading their text — the distinction `pdn-node-http`'s table
    /// folds away and this one keeps.
    #[test]
    fn the_ceremony_outcomes_do_not_collapse() {
        assert!(matches!(
            kind(InviterUnreachable),
            PdnError::CounterpartyUnreachable
        ));
        assert!(matches!(
            kind(EstablishmentTimeout),
            PdnError::DialogueTimedOut
        ));
        assert!(matches!(kind(DialogueTimeout), PdnError::DialogueTimedOut));
        assert!(matches!(kind(CatchUpTimeout), PdnError::CatchUpTimedOut));
        assert!(matches!(
            kind(UnsupportedInviteVersion { version: 9 }),
            PdnError::UnsupportedPayloadVersion { version: 9 }
        ));
        assert!(matches!(
            kind(UnsupportedLinkingVersion { version: 9 }),
            PdnError::UnsupportedPayloadVersion { version: 9 }
        ));
    }

    /// The pessimistic default: what the table cannot name it does not
    /// launder into a refusal, and the cause chain does not travel.
    #[test]
    fn an_unmapped_error_is_internal_and_carries_no_cause() {
        let err = kind(anyhow!("private storage path /secret/node.db"));
        assert!(matches!(err, PdnError::Internal));
        let text = err.to_string();
        assert!(!text.contains("/secret/node.db"), "{text}");
    }

    /// Service errors arrive wrapped in context; the table reads through
    /// it.
    #[test]
    fn a_wrapped_refusal_keeps_its_kind() {
        let wrapped = Err::<(), _>(anyhow::Error::new(LinkingRefused))
            .context("linking into the identity")
            .unwrap_err();
        assert!(matches!(table(&wrapped), PdnError::CeremonyRefused));
    }

    /// A refusal names what was refused, so a screen can say it.
    #[test]
    fn a_refusal_names_its_subject() {
        let err = kind(UnknownIdentity { identity: PEER });
        assert!(err.to_string().contains(&PEER.to_string()));
    }

    /// The host's own kinds, decided before the runtime is called.
    #[test]
    fn the_hosts_own_kinds() {
        assert!(matches!(
            PdnError::malformed("empty payload"),
            PdnError::MalformedInput { .. }
        ));
    }
}
