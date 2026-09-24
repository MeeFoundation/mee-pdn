"""
Claude-focused mitmproxy addon: network access rules and secret substitution.

Loaded via: mitmweb -s /scripts/mitmproxy_addon_claude.py

This is a thin wrapper around the shared :mod:`mitmproxy_addon_common`
library. Claude does not require streaming-aware handling, so the default
behaviour of the base ``SandcatAddon`` class is sufficient.

On startup, reads settings from up to three layers (lowest to highest
precedence): user (``~/.config/sandcat/settings.json``), project
(``.sandcat/settings.json``), and local (``.sandcat/settings.local.json``).
Env vars and secrets are merged (higher precedence wins on conflict).
Network rules are concatenated (highest precedence first).

Network rules are evaluated top-to-bottom, first match wins, default deny.
Secret placeholders are replaced with real values only for allowed hosts.
"""

from mitmproxy_addon_common import SandcatAddon

addons = [SandcatAddon()]
