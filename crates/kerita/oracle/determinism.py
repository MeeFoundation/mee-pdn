"""ECDSA signatures from keripy, made deterministic (RFC 6979).

keripy signs P-256 with a random nonce, so the same event signed twice gives
two signatures and a corpus holding one would differ from every regeneration.
Only keripy's signing module sees the deterministic nonce; its verification
is untouched, and a deterministic signature is as valid as a random one.
"""
import types

from cryptography.hazmat.primitives.asymmetric import ec

from keri.core import signing


def install():
    deterministic = types.SimpleNamespace(**{k: getattr(ec, k) for k in dir(ec)
                                             if not k.startswith("__")})
    deterministic.ECDSA = lambda algorithm: ec.ECDSA(algorithm, deterministic_signing=True)
    signing.ec = deterministic
