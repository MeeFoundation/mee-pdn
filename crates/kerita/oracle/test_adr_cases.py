"""The cases of ADR-0017 the crate is held to, built with keripy on P-256 keys.

Each test is one validator's view of Alice's identity KEL, or of Erin's; the
harvest records every event the validator is given and what keripy answers.
Seeds are fixed and ECDSA signing is deterministic (determinism.py), so a
regeneration writes the same bytes.
"""
import contextlib
import hashlib

from keri.core import eventing, signing
from keri.core.coring import Diger, MtrDex, Number
from keri.db import basing
from keri.kering import Kinds, TraitDex, Vrsn_2_0

V2 = dict(version=Vrsn_2_0, kind=Kinds.cesr)


def _seed(name):
    return hashlib.blake2b(name.encode(), digest_size=32).digest()


def p256(name):
    return signing.Signer(raw=_seed(name), code=MtrDex.ECDSA_256r1_Seed, transferable=True)


def witness(name):
    return signing.Signer(raw=_seed(name), code=MtrDex.Ed25519_Seed, transferable=False)


def dig(signer):
    return Diger(ser=signer.verfer.qb64b).qb64


def key(signer):
    return signer.verfer.qb64


PHONE = [p256(f"alice-phone-{n}") for n in range(6)]
PAPER = [p256(f"alice-paper-{n}") for n in range(2)]
LAPTOP = [p256(f"alice-laptop-{n}") for n in range(4)]
PHONE_DEVICE = [p256(f"alice-phone-device-{n}") for n in range(2)]
LAPTOP_DEVICE = [p256(f"alice-laptop-device-{n}") for n in range(2)]
W_TELLIPOD, W_HOME, W_RIVAL = witness("w-tellipod"), witness("w-home"), witness("w-rival")
MALLORY = p256("mallory")


class Event:
    def __init__(self, serder, sigers, wigers=(), source=None):
        self.serder, self.sigers, self.wigers, self.source = serder, list(sigers), list(wigers), source


def _alice():
    """Alice's identity KEL and her 2 device AIDs, as ADR-0017 lays them out."""
    icp = eventing.incept(keys=[key(PHONE[0]), key(PAPER[0])], isith=["1", "1"],
                          ndigs=[dig(PHONE[1]), dig(PAPER[1])], nsith=["1", "1"],
                          wits=[key(W_TELLIPOD)], toad=1, cnfg=[TraitDex.EstOnly],
                          code=MtrDex.Blake3_256, **V2)
    pre = icp.pre
    phone_dip = eventing.delcept(keys=[key(PHONE_DEVICE[0])], delpre=pre,
                                 ndigs=[dig(PHONE_DEVICE[1])], **V2)
    laptop_dip = eventing.delcept(keys=[key(LAPTOP_DEVICE[0])], delpre=pre,
                                  ndigs=[dig(LAPTOP_DEVICE[1])], **V2)
    wits = [key(W_TELLIPOD)]
    # Rotation 1 approves the phone's device AID; it cannot sit in the inception (DR-6).
    rot1 = eventing.rotate(pre=pre, keys=[key(PHONE[1])], dig=icp.said, sn=1, isith=["1"],
                           ndigs=[dig(PHONE[2]), dig(PAPER[1])], nsith=["1", "1"],
                           wits=wits, toad=1,
                           data=[dict(i=phone_dip.pre, s="0", d=phone_dip.said)], **V2)
    # Rotation 2 links the laptop: its next-key digest joins, its device AID is approved.
    rot2 = eventing.rotate(pre=pre, keys=[key(PHONE[2])], dig=rot1.said, sn=2, isith=["1"],
                           ndigs=[dig(PHONE[3]), dig(PAPER[1]), dig(LAPTOP[0])],
                           nsith=["1/2", "1", "1/2"], wits=wits, toad=1,
                           data=[dict(i=laptop_dip.pre, s="0", d=laptop_dip.said)], **V2)
    # Rotation 3: laptop and phone sign at prior-next slots 2 and 0, current 0 and 1.
    rot3 = eventing.rotate(pre=pre, keys=[key(LAPTOP[0]), key(PHONE[3])], dig=rot2.said, sn=3,
                           isith=["1/2", "1/2"],
                           ndigs=[dig(LAPTOP[1]), dig(PHONE[4]), dig(PAPER[1])],
                           nsith=["1/2", "1/2", "1"], wits=wits, toad=1, **V2)
    # Rotation 3 as Mallory forges it: her key at the phone's slot, never committed.
    forged = eventing.rotate(pre=pre, keys=[key(LAPTOP[0]), key(MALLORY)], dig=rot2.said, sn=3,
                             isith=["1/2", "1/2"],
                             ndigs=[dig(LAPTOP[1]), dig(PHONE[4]), dig(PAPER[1])],
                             nsith=["1/2", "1/2", "1"], wits=wits, toad=1, **V2)
    # Rotation 3 by the paper and a key never committed: the paper's exposure meets the prior
    # next threshold, the new key's signature the current one beside the paper's.
    renewed = eventing.rotate(pre=pre, keys=[key(PAPER[1]), key(MALLORY)], dig=rot2.said, sn=3,
                              isith=["1/2", "1/2"],
                              ndigs=[dig(LAPTOP[1]), dig(PHONE[4]), dig(PAPER[1])],
                              nsith=["1/2", "1/2", "1"], wits=wits, toad=1, **V2)
    # Rotation 4 moves Alice from W-tellipod to W-home; its rival adds W-rival instead.
    rot4 = eventing.rotate(pre=pre, keys=[key(LAPTOP[1]), key(PHONE[4])], dig=rot3.said, sn=4,
                           isith=["1/2", "1/2"],
                           ndigs=[dig(LAPTOP[2]), dig(PHONE[5]), dig(PAPER[1])],
                           nsith=["1/2", "1/2", "1"], wits=wits, cuts=wits,
                           adds=[key(W_HOME)], toad=1, **V2)
    rival = eventing.rotate(pre=pre, keys=[key(LAPTOP[1]), key(PHONE[4])], dig=rot3.said, sn=4,
                            isith=["1/2", "1/2"],
                            ndigs=[dig(LAPTOP[2]), dig(PHONE[5]), dig(PAPER[1])],
                            nsith=["1/2", "1/2", "1"], wits=wits, cuts=wits,
                            adds=[key(W_RIVAL)], toad=1, **V2)
    ixn = eventing.interact(pre=pre, dig=rot4.said, sn=5, **V2)

    def signed(serder, *signers):
        return [s.sign(serder.raw, index=i, ondex=o) for s, i, o in signers]

    def receipt(serder, w):
        return [w.sign(serder.raw, index=0)]

    return {
        "icp-bare": Event(icp, signed(icp, (PHONE[0], 0, None))),
        "icp": Event(icp, signed(icp, (PHONE[0], 0, None)), receipt(icp, W_TELLIPOD)),
        "phone-dip": Event(phone_dip, signed(phone_dip, (PHONE_DEVICE[0], 0, None))),
        "rot1": Event(rot1, signed(rot1, (PHONE[1], 0, 0)), receipt(rot1, W_TELLIPOD)),
        "rot2": Event(rot2, signed(rot2, (PHONE[2], 0, 0)), receipt(rot2, W_TELLIPOD)),
        "laptop-dip": Event(laptop_dip, signed(laptop_dip, (LAPTOP_DEVICE[0], 0, None)),
                            source=(2, rot2.said)),
        "rot3-short": Event(rot3, signed(rot3, (LAPTOP[0], 0, 2)), receipt(rot3, W_TELLIPOD)),
        "rot3-forged": Event(forged, signed(forged, (LAPTOP[0], 0, 2), (MALLORY, 1, 0)),
                             receipt(forged, W_TELLIPOD)),
        "rot3": Event(rot3, signed(rot3, (LAPTOP[0], 0, 2), (PHONE[3], 1, 0)),
                      receipt(rot3, W_TELLIPOD)),
        "rot3-renewed-alone": Event(renewed, signed(renewed, (PAPER[1], 0, 1)),
                                    receipt(renewed, W_TELLIPOD)),
        "rot3-renewed": Event(renewed, signed(renewed, (PAPER[1], 0, 1), (MALLORY, 1, 1)),
                              receipt(renewed, W_TELLIPOD)),
        "rot3-renewed-current": Event(renewed, [*signed(renewed, (PAPER[1], 0, 1)),
                                                MALLORY.sign(renewed.raw, index=1, only=True)],
                                      receipt(renewed, W_TELLIPOD)),
        "rot4-cut-receipt": Event(rot4, signed(rot4, (LAPTOP[1], 0, 0), (PHONE[4], 1, 1)),
                                  receipt(rot4, W_TELLIPOD)),
        "rot4": Event(rot4, signed(rot4, (LAPTOP[1], 0, 0), (PHONE[4], 1, 1)),
                      receipt(rot4, W_HOME)),
        "rival": Event(rival, signed(rival, (LAPTOP[1], 0, 0), (PHONE[4], 1, 1)),
                       receipt(rival, W_RIVAL)),
        "ixn": Event(ixn, signed(ixn, (LAPTOP[2], 0, None)), receipt(ixn, W_HOME)),
    }


ALICE = _alice()


@contextlib.contextmanager
def validator(name, controls=()):
    """A validator, controlling the AIDs `controls` names."""
    with basing.openDB(name=name, temp=True) as db:
        for pre in controls:
            db.prefixes.add(pre)
        yield eventing.Kevery(db=db, lax=False, local=False)


def give(kevery, event, local=False):
    """Gives a validator one event, from its own source when `local`; keripy's answer is what
    the harvest records."""
    source = ({} if event.source is None else
              dict(delsner=Number(num=event.source[0]), delsger=Diger(qb64=event.source[1])))
    with contextlib.suppress(Exception):
        kevery.processEvent(serder=event.serder, sigers=event.sigers, wigers=event.wigers,
                            local=local, **source)


def retry(kevery):
    """keripy gives every escrowed event to its validator again."""
    with contextlib.suppress(Exception):
        kevery.processEscrows()


def test_identity_in_order():
    """Bob's validator: Alice's KEL in order, a device AID before its approval, the rival
    rotation and an interaction event in her establishment-only KEL."""
    with validator("bob") as kevery:
        give(kevery, ALICE["icp-bare"])
        give(kevery, ALICE["icp"])
        give(kevery, ALICE["phone-dip"])
        give(kevery, ALICE["rot1"])
        retry(kevery)
        give(kevery, ALICE["rot2"])
        give(kevery, ALICE["laptop-dip"])
        give(kevery, ALICE["rot3"])
        give(kevery, ALICE["rot4"])
        give(kevery, ALICE["rival"])
        give(kevery, ALICE["ixn"])


def test_one_signer_short():
    """Carol's validator: rotation 3 with the laptop's signature alone waits, and so does one
    whose second signer's key was never committed; with both of Alice's signers it is accepted."""
    with validator("carol") as kevery:
        for name in ("icp", "rot1", "rot2", "rot3-short", "rot3-forged"):
            give(kevery, ALICE[name])
        give(kevery, ALICE["rot3"])


def test_new_key_signs_toward_the_current_threshold():
    """Ivan's validator: rotation 3 by the paper alone waits; with a key never committed signing
    beside it, at the paper's prior-next index, it is accepted: that signature counts toward the
    current threshold and not the prior next one."""
    with validator("ivan") as kevery:
        for name in ("icp", "rot1", "rot2", "rot3-renewed-alone"):
            give(kevery, ALICE[name])
        give(kevery, ALICE["rot3-renewed"])


def test_new_key_signs_in_the_current_list_alone():
    """Judy's validator: the same rotation with the new key's signature indexed into the current
    keys alone is accepted."""
    with validator("judy") as kevery:
        for name in ("icp", "rot1", "rot2", "rot3-renewed-current"):
            give(kevery, ALICE[name])


def test_cut_witness_receipt():
    """Dave's validator: rotation 4 receipted by the witness it cuts waits; by the one it
    adds, it is accepted."""
    with validator("dave") as kevery:
        for name in ("icp", "rot1", "rot2", "rot3", "rot4-cut-receipt"):
            give(kevery, ALICE[name])
        give(kevery, ALICE["rot4"])


def test_controller_accepts_its_own_events_without_receipts():
    """Alice's phone, her identity's controller: her inception and rotation 1, made on the phone,
    are accepted without W-tellipod's receipts. Paired: Bob's validator, given the same rotation
    from the network, waits for them."""
    icp, rot1 = ALICE["icp"], ALICE["rot1"]
    bare = Event(rot1.serder, rot1.sigers)
    with validator("alice-phone", controls=[icp.serder.pre]) as kevery:
        give(kevery, Event(icp.serder, icp.sigers), local=True)
        give(kevery, bare, local=True)
    with validator("bob-remote") as kevery:
        give(kevery, icp)
        give(kevery, bare)


def test_out_of_order():
    """Frank's validator: rotation 2 before rotation 1 waits, and is accepted once
    rotation 1 is."""
    with validator("frank") as kevery:
        give(kevery, ALICE["icp"])
        give(kevery, ALICE["rot2"])
        give(kevery, ALICE["rot1"])
        retry(kevery)


def _erin():
    """Erin's KEL, whose rotation 1 anchors a digest seal of her device AID's inception."""
    device = [p256(f"erin-device-{n}") for n in range(2)]
    keys = [p256(f"erin-{n}") for n in range(3)]
    icp = eventing.incept(keys=[key(keys[0])], ndigs=[dig(keys[1])], cnfg=[TraitDex.EstOnly],
                          code=MtrDex.Blake3_256, **V2)
    dip = eventing.delcept(keys=[key(device[0])], delpre=icp.pre, ndigs=[dig(device[1])], **V2)
    rot1 = eventing.rotate(pre=icp.pre, keys=[key(keys[1])], dig=icp.said, sn=1,
                           ndigs=[dig(keys[2])], data=[dict(d=dip.said)], **V2)
    return (Event(icp, [keys[0].sign(icp.raw, index=0)]),
            Event(dip, [device[0].sign(dip.raw, index=0)]),
            Event(rot1, [keys[1].sign(rot1.raw, index=0, ondex=0)]))


def test_digest_seal_approves_nothing():
    """Grace's validator: Erin's device AID, sealed by digest alone, waits for good."""
    icp, dip, rot1 = _erin()
    with validator("grace") as kevery:
        give(kevery, icp)
        give(kevery, dip)
        give(kevery, rot1)
        retry(kevery)


# The order of the P-256 group: a signature (r, s) verifies as (r, n - s) too.
P256_ORDER = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551


def test_high_s_signature():
    """Heidi's validator: an inception whose P-256 signature carries the high S value."""
    from keri.core.indexing import Siger

    keys = [p256(f"heidi-{n}") for n in range(2)]
    icp = eventing.incept(keys=[key(keys[0])], ndigs=[dig(keys[1])], code=MtrDex.Blake3_256, **V2)
    low = keys[0].sign(icp.raw, index=0)
    r, s = low.raw[:32], int.from_bytes(low.raw[32:], "big")
    high = max(s, P256_ORDER - s).to_bytes(32, "big")
    siger = Siger(raw=r + high, code=low.code, index=low.index, ondex=low.ondex,
                  verfer=low.verfer)
    with validator("heidi") as kevery:
        give(kevery, Event(icp, [siger]))
