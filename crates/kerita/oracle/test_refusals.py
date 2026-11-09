"""Inputs the crate refuses, built with keripy: broken events, and events or KELs outside the
subset. What keripy answers is what the harvest records; what the crate answers follows from
the vector's expectation (corpus.py).
"""
import contextlib

from keri.core import coring, counting, eventing, serdering, signing
from keri.core.coring import Diger, MtrDex
from keri.kering import TraitDex, Vrsn_2_0

import test_adr_cases as adr

ICP = adr.ALICE["icp"].serder


def parse(raw):
    with contextlib.suppress(Exception):
        serdering.SerderKERI(raw=bytes(raw))


def test_wrong_said():
    """Alice's inception with the last character of its SAID changed."""
    raw = bytearray(ICP.raw)
    at = raw.find(ICP.said.encode()) + len(ICP.said) - 1
    raw[at] = ord("B") if raw[at] != ord("B") else ord("C")
    parse(raw)


def test_group_cut_short():
    """Alice's inception cut in the middle of its key list."""
    parse(ICP.raw[:100])


def test_group_above_limit():
    """Alice's inception with its key list declared 4,095 quadlets long."""
    keys = counting.Counter(code=counting.Codens.GenericListGroup, count=24, version=Vrsn_2_0)
    assert keys.qb64b in ICP.raw
    parse(ICP.raw.replace(keys.qb64b, b"-J__", 1))


def _incept(**kwa):
    signers = [adr.p256(f"outside-{n}") for n in range(4)]
    serder = eventing.incept(keys=[adr.key(s) for s in signers[:3]], ndigs=[adr.dig(signers[3])],
                             code=MtrDex.Blake3_256, **adr.V2, **kwa)
    return adr.Event(serder, [s.sign(serder.raw, index=i) for i, s in enumerate(signers[:3])])


def test_nested_weight_map():
    """An inception whose current threshold is a nested weight map."""
    with adr.validator("nested") as kevery:
        adr.give(kevery, _incept(isith=[{"1/2": ["1/2", "1/2"]}, "1/2"]))


def test_dnd_trait():
    """An inception carrying the `DND` trait."""
    with adr.validator("dnd") as kevery:
        adr.give(kevery, _incept(isith="2", cnfg=[TraitDex.DoNotDelegate]))


def test_trait_in_rotation():
    """A rotation carrying a configuration trait."""
    keys = [adr.p256(f"rotated-{n}") for n in range(3)]
    icp = eventing.incept(keys=[adr.key(keys[0])], ndigs=[adr.dig(keys[1])],
                          code=MtrDex.Blake3_256, **adr.V2)
    rot = eventing.rotate(pre=icp.pre, keys=[adr.key(keys[1])], dig=icp.said, sn=1,
                          ndigs=[adr.dig(keys[2])], cnfg=[TraitDex.NoRegistrarBackers], **adr.V2)
    with adr.validator("rotation-trait") as kevery:
        adr.give(kevery, adr.Event(icp, [keys[0].sign(icp.raw, index=0)]))
        adr.give(kevery, adr.Event(rot, [keys[1].sign(rot.raw, index=0, ondex=0)]))


def test_witness_threshold_above_list():
    """Alice's inception with its witness threshold raised to 2 over her one witness."""
    sad = dict(ICP.sad)
    sad["bt"] = "2"
    serder = serdering.SerderKERI(sad=sad, makify=True)
    with adr.validator("toad") as kevery:
        adr.give(kevery, adr.Event(serder, [adr.PHONE[0].sign(serder.raw, index=0)],
                                   [adr.W_TELLIPOD.sign(serder.raw, index=0)]))


def _witnessed_by(witness_prefix, witness_signer):
    keys = [adr.p256(f"transferable-witness-{n}") for n in range(2)]
    icp = eventing.incept(keys=[adr.key(keys[0])], ndigs=[adr.dig(keys[1])],
                          wits=[witness_prefix], toad=1, code=MtrDex.Blake3_256, **adr.V2)
    return adr.Event(icp, [keys[0].sign(icp.raw, index=0)],
                     [witness_signer.sign(icp.raw, index=0)])


def test_transferable_basic_witness():
    """An inception witnessed by a transferable basic AID — its prefix its Ed25519 key — that
    receipts it."""
    witness = signing.Signer(raw=adr._seed("transferable-witness"), code=MtrDex.Ed25519_Seed,
                             transferable=True)
    with adr.validator("transferable-witness") as kevery:
        adr.give(kevery, _witnessed_by(witness.verfer.qb64, witness))


def test_self_addressing_witness():
    """An inception witnessed by a self-addressing AID, which no key can stand for."""
    witness = signing.Signer(raw=adr._seed("self-addressing-witness"),
                             code=MtrDex.Ed25519_Seed, transferable=True)
    prefix = adr.dig(witness)
    with adr.validator("self-addressing-witness") as kevery:
        adr.give(kevery, _witnessed_by(prefix, witness))


def _abandoned():
    """An AID whose rotation 1 commits to no next key, abandoning it."""
    keys = [adr.p256(f"abandoned-{n}") for n in range(3)]
    icp = eventing.incept(keys=[adr.key(keys[0])], ndigs=[adr.dig(keys[1])],
                          code=MtrDex.Blake3_256, **adr.V2)
    rot1 = eventing.rotate(pre=icp.pre, keys=[adr.key(keys[1])], dig=icp.said, sn=1, ndigs=[],
                           nsith="0", **adr.V2)
    return keys, [adr.Event(icp, [keys[0].sign(icp.raw, index=0)]),
                  adr.Event(rot1, [keys[1].sign(rot1.raw, index=0, ondex=0)])]


THIEF = adr.p256("thief")


def test_rotation_of_an_abandoned_aid():
    """An abandoned AID rotated by a key it never committed to."""
    keys, events = _abandoned()
    rot1 = events[-1].serder
    rot2 = eventing.rotate(pre=rot1.pre, keys=[adr.key(THIEF)], dig=rot1.said, sn=2,
                           ndigs=[adr.dig(keys[2])], **adr.V2)
    with adr.validator("abandoned-rotation") as kevery:
        for event in events:
            adr.give(kevery, event)
        adr.give(kevery, adr.Event(rot2, [THIEF.sign(rot2.raw, index=0, only=True)]))


def test_interaction_of_an_abandoned_aid():
    """An abandoned AID's interaction event, signed by its last current key."""
    keys, events = _abandoned()
    rot1 = events[-1].serder
    ixn = eventing.interact(pre=rot1.pre, dig=rot1.said, sn=2, **adr.V2)
    with adr.validator("abandoned-interaction") as kevery:
        for event in events:
            adr.give(kevery, event)
        adr.give(kevery, adr.Event(ixn, [keys[1].sign(ixn.raw, index=0)]))


def test_rotation_past_a_next_threshold_of_zero():
    """An AID whose next threshold is 0 over a committed digest, rotated by a key it never
    committed to."""
    keys = [adr.p256(f"zero-next-{n}") for n in range(3)]
    icp = eventing.incept(keys=[adr.key(keys[0])], ndigs=[adr.dig(keys[1])], nsith="0",
                          code=MtrDex.Blake3_256, **adr.V2)
    rot1 = eventing.rotate(pre=icp.pre, keys=[adr.key(THIEF)], dig=icp.said, sn=1,
                           ndigs=[adr.dig(keys[2])], **adr.V2)
    with adr.validator("zero-next") as kevery:
        adr.give(kevery, adr.Event(icp, [keys[0].sign(icp.raw, index=0)]))
        adr.give(kevery, adr.Event(rot1, [THIEF.sign(rot1.raw, index=0, only=True)]))


def test_rotation_of_a_non_transferable_aid():
    """A non-transferable basic AID rotated by a key it never committed to."""
    owner = adr.witness("non-transferable")
    icp = eventing.incept(keys=[adr.key(owner)], code=MtrDex.Ed25519N, **adr.V2)
    rot1 = eventing.rotate(pre=icp.pre, keys=[adr.key(THIEF)], dig=icp.said, sn=1, ndigs=[],
                           nsith="0", **adr.V2)
    with adr.validator("non-transferable") as kevery:
        adr.give(kevery, adr.Event(icp, [owner.sign(icp.raw, index=0)]))
        adr.give(kevery, adr.Event(rot1, [THIEF.sign(rot1.raw, index=0, only=True)]))


B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"


def _recoded(serder, anchor, old, new):
    """The event's text with `old`, where it follows `anchor`, written as `new`, its body length
    and SAID made to fit: bytes keripy's writer never produces from any event."""
    raw = bytes(serder.raw).decode()
    at = raw.index(anchor + old) + len(anchor)
    text = raw[:at] + new + raw[at + len(old):]
    quadlets = (len(text) - 4) // 4
    text = "-F" + B64[quadlets >> 6] + B64[quadlets & 63] + text[4:]
    self_addressing = serder.pre == serder.said
    dummied = text[:20] + "#" * 44 + ("#" * 44 if self_addressing else text[64:108]) + text[108:]
    said = Diger(ser=dummied.encode(), code=MtrDex.Blake3_256).qb64
    return text[:20] + said + (said if self_addressing else text[64:108]) + text[108:]


def _one_key(**kwa):
    keys = [adr.p256(f"recoded-{n}") for n in range(3)]
    return eventing.incept(keys=[adr.key(keys[0])], ndigs=[adr.dig(keys[1])],
                           code=MtrDex.Blake3_256, **adr.V2, **kwa), keys


def test_next_threshold_no_signers_meet():
    """An inception whose next threshold is the one clause ["1/2"], which no signers meet."""
    icp, keys = _one_key()
    parse(_recoded(icp, adr.key(keys[0]), "MAAB", coring.Bexter(bext="1s2").qb64).encode())


def test_current_threshold_no_signers_meet():
    """An inception whose current threshold is the one clause ["1/2"], which no signers meet."""
    icp, _ = _one_key()
    parse(_recoded(icp, icp.pre + "MAAA", "MAAB", coring.Bexter(bext="1s2").qb64).encode())


def test_sequence_number_under_a_longer_code():
    """keripy's one-key inception with its sequence number 0 written under `0H`, 8 characters
    where `MAAA` takes 4."""
    icp, _ = _one_key()
    parse(_recoded(icp, icp.pre, "MAAA", "0HAAAAAA").encode())


def test_key_list_under_a_big_count_code():
    """keripy's one-key inception with its key list under the big count code `--J`, where the
    small one holds it."""
    icp, keys = _one_key()
    header = counting.Counter(code=counting.Codens.GenericListGroup, count=12,
                              version=Vrsn_2_0).qb64
    parse(_recoded(icp, "MAAB", header + adr.key(keys[0]),
                   "--JAAAAM" + adr.key(keys[0])).encode())


def test_digest_seals_split_into_two_groups():
    """An inception anchoring 2 digest seals in 2 groups of one, where keripy writes one group of
    both."""
    first, second = (Diger(ser=name.encode(), code=MtrDex.Blake3_256).qb64
                     for name in ("seal-1", "seal-2"))
    icp, _ = _one_key(data=[dict(d=first), dict(d=second)])
    parse(_recoded(icp, "", "-JAX-QAW" + first + second,
                   "-JAY-QAL" + first + "-QAL" + second).encode())


def test_weight_not_reduced():
    """An inception whose current threshold holds the weight 2/4, which keripy writes 1/2."""
    keys = [adr.p256(f"unreduced-{n}") for n in range(4)]
    icp = eventing.incept(keys=[adr.key(keys[0]), adr.key(keys[1])], isith=["1/2", "1/2"],
                          ndigs=[adr.dig(keys[2]), adr.dig(keys[3])],
                          code=MtrDex.Blake3_256, **adr.V2)
    parse(_recoded(icp, icp.pre + "MAAA", coring.Bexter(bext="1s2c1s2").qb64,
                   coring.Bexter(bext="2s4c1s2").qb64).encode())
