"""
Vitro proxy policy — pure functions, no mitmproxy dependency.

The mitmproxy addon (vitro_addon.py) adapts mitmproxy events to calls
into this module. All security-critical decisions (allow/deny, credential
injection, passthrough) live here so they can be unit-tested without
booting mitmproxy.
"""

from __future__ import annotations

from pathlib import Path
from typing import Iterable, Mapping, Sequence


DEFAULT_READ_METHODS = ("GET", "HEAD", "OPTIONS")


def matches_domain(hostname: str, pattern: str) -> bool:
    h = hostname.lower()
    p = pattern.lower()
    if p.startswith("*."):
        return h.endswith(f".{p[2:]}") or h == p[2:]
    return h == p


def matches_any(hostname: str, domains) -> bool:
    if domains == "*":
        return True
    if isinstance(domains, list):
        return any(matches_domain(hostname, d) for d in domains)
    return False


def index_cells_by_ip(cells_list) -> dict:
    """Index a list of cell-rule dicts by `cellIp`. Entries without a
    cellIp are dropped. Later entries overwrite earlier on collision."""
    out: dict = {}
    if not isinstance(cells_list, list):
        return out
    for cell in cells_list:
        ip = (cell or {}).get("cellIp", "")
        if ip:
            out[ip] = cell
    return out


def has_specific_deny(hostname: str, denied) -> bool:
    """True iff `hostname` matches a non-wildcard entry in `denied`.

    The literal `"*"` is treated as "default deny everything" — it
    establishes a policy floor but is overridable by explicit allow.
    Per-pattern entries (including domain wildcards like `*.evil.com`)
    are specific denies that always win over allow.
    """
    if denied == "*":
        return False
    if isinstance(denied, list):
        return any(d != "*" and matches_domain(hostname, d) for d in denied)
    return False


def classify_method(method: str, read_methods: Iterable[str] = DEFAULT_READ_METHODS) -> str:
    return "reads" if method in read_methods else "writes"


def normalize_client_ip(client_ip: str) -> str:
    return client_ip.removeprefix("::ffff:")


def merge_rules(global_rules: dict, cell_egress: dict | None, direction: str) -> tuple:
    """Resolve effective (allowed, denied) for a cell + direction.

    Cell-level rules either layer on top of global (additive) or replace
    them entirely (additive=False). Cell `allowed` overrides global; cell
    `denied` adds to global denied when additive.
    """
    rules = global_rules.get(direction, {}) or {}
    global_allowed = rules.get("allowed", [])
    global_denied = rules.get("denied", [])

    cell_egress = cell_egress or {}
    additive = cell_egress.get("additive", True)
    cell_rules = cell_egress.get(direction) or {}
    cell_allowed = cell_rules.get("allowed")
    cell_denied = cell_rules.get("denied")

    if cell_allowed is None and cell_denied is None:
        return global_allowed, global_denied

    if additive:
        allowed = cell_allowed if cell_allowed is not None else global_allowed
        if cell_denied:
            if isinstance(global_denied, list):
                denied = global_denied + cell_denied
            else:
                denied = cell_denied
        else:
            denied = global_denied
        return allowed, denied

    return (cell_allowed or []), (cell_denied or [])


def is_allowed(
    client_ip: str,
    host: str,
    method: str,
    cells: Mapping[str, dict],
    egress: dict,
    read_methods: Iterable[str] | None = None,
) -> bool:
    """Decide whether (client_ip → host, method) may proceed.

    Unknown clients are denied. Otherwise, classify the method, resolve
    effective rules for the cell, and apply them with `denied` taking
    precedence unless `allowed` also matches (explicit override).
    """
    ip = normalize_client_ip(client_ip)
    if ip not in cells:
        return False

    cell = cells[ip]
    direction = classify_method(
        method,
        read_methods or egress.get("reads", {}).get("methods") or DEFAULT_READ_METHODS,
    )
    allowed, denied = merge_rules(egress, cell.get("egress"), direction)

    # Specific (non-wildcard) denials always win — even over an explicit
    # allow. Wildcard "*" in `denied` is just default-deny and is
    # overridable by the allowlist.
    if has_specific_deny(host, denied):
        return False
    return matches_any(host, allowed)


def collect_credentials(
    cells: Mapping[str, dict],
    client_ip: str,
    global_credentials: Sequence[dict],
) -> list[dict]:
    creds = list(global_credentials)
    ip = normalize_client_ip(client_ip)
    if ip in cells:
        cell_egress = cells[ip].get("egress") or {}
        creds.extend(cell_egress.get("credentials", []))
    return creds


def credential_value(env_var: str, secrets: Mapping[str, str], environ: Mapping[str, str]) -> str:
    return secrets.get(env_var, "") or environ.get(env_var, "")


def compute_injected_headers(
    host: str,
    credentials: Sequence[dict],
    secrets: Mapping[str, str],
    environ: Mapping[str, str],
) -> dict[str, str]:
    """Return headers to inject for a request to `host`.

    Picks credentials whose `host` matches exactly or as a parent domain.
    Authorization headers get a `Bearer ` prefix if missing.
    """
    out: dict[str, str] = {}
    host_lc = host.lower()
    for cred in credentials:
        cred_host = cred["host"].lower()
        if host_lc == cred_host or host_lc.endswith(f".{cred_host}"):
            value = credential_value(cred["envVar"], secrets, environ)
            if not value:
                continue
            header = cred["header"]
            if header.lower() == "authorization" and not value.startswith("Bearer "):
                value = f"Bearer {value}"
            out[header] = value
    return out


def parse_secrets_env(text: str) -> dict[str, str]:
    """Parse a KEY=value file. Skips blanks and `#` comments."""
    out: dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "=" in line:
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip()
    return out


def load_secrets_env(path: Path) -> dict[str, str]:
    if not path.exists():
        return {}
    return parse_secrets_env(path.read_text())
