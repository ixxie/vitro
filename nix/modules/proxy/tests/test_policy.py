# /// script
# requires-python = ">=3.11"
# dependencies = ["pytest"]
# ///
"""
Pure-logic tests for vitro_policy.

Run: cd nix/modules/proxy && uv run --script tests/test_policy.py
Or:  cd nix/modules/proxy && pytest tests/
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import vitro_policy as p


# ---- domain matching --------------------------------------------------------


@pytest.mark.parametrize(
    "host,pattern,want",
    [
        ("api.linear.app", "api.linear.app", True),
        ("API.linear.app", "api.linear.app", True),
        ("api.linear.app", "*.linear.app", True),
        ("linear.app", "*.linear.app", True),
        ("evil.com", "linear.app", False),
        ("api-linear.app", "*.linear.app", False),
        ("a.b.linear.app", "*.linear.app", True),
    ],
)
def test_matches_domain(host, pattern, want):
    assert p.matches_domain(host, pattern) is want


def test_matches_any_wildcard_string():
    assert p.matches_any("anything.example", "*") is True


def test_matches_any_list():
    assert p.matches_any("api.linear.app", ["github.com", "api.linear.app"]) is True
    assert p.matches_any("evil.com", ["github.com"]) is False


def test_matches_any_non_list_non_wildcard():
    assert p.matches_any("a.com", None) is False
    assert p.matches_any("a.com", {}) is False


# ---- classify_method --------------------------------------------------------


def test_classify_method_default():
    assert p.classify_method("GET") == "reads"
    assert p.classify_method("HEAD") == "reads"
    assert p.classify_method("OPTIONS") == "reads"
    assert p.classify_method("POST") == "writes"
    assert p.classify_method("PUT") == "writes"
    assert p.classify_method("DELETE") == "writes"


def test_classify_method_custom():
    # if config marks PROPFIND a read, treat it as one
    assert p.classify_method("PROPFIND", read_methods=["GET", "PROPFIND"]) == "reads"


# ---- normalize ip -----------------------------------------------------------


def test_normalize_v4_mapped_v6():
    assert p.normalize_client_ip("::ffff:192.168.83.42") == "192.168.83.42"
    assert p.normalize_client_ip("10.0.0.1") == "10.0.0.1"


# ---- merge_rules ------------------------------------------------------------


def test_merge_rules_no_cell_overrides():
    egress = {"reads": {"allowed": "*", "denied": []}}
    a, d = p.merge_rules(egress, None, "reads")
    assert a == "*" and d == []


def test_merge_rules_additive_allowed_replaces_global_allowed():
    egress = {"writes": {"allowed": ["github.com"], "denied": []}}
    env = {"additive": True, "writes": {"allowed": ["api.linear.app"]}}
    a, d = p.merge_rules(egress, env, "writes")
    assert a == ["api.linear.app"]
    assert d == []


def test_merge_rules_additive_denied_appends():
    egress = {"writes": {"allowed": ["github.com"], "denied": ["evil.com"]}}
    env = {"additive": True, "writes": {"denied": ["worse.com"]}}
    a, d = p.merge_rules(egress, env, "writes")
    assert a == ["github.com"]
    assert d == ["evil.com", "worse.com"]


def test_merge_rules_replace_mode():
    egress = {"writes": {"allowed": ["github.com"], "denied": ["evil.com"]}}
    env = {"additive": False, "writes": {"allowed": ["api.linear.app"]}}
    a, d = p.merge_rules(egress, env, "writes")
    assert a == ["api.linear.app"]
    assert d == []


# ---- is_allowed -------------------------------------------------------------


def base_egress():
    return {
        "reads": {"methods": ["GET", "HEAD", "OPTIONS"], "allowed": "*", "denied": []},
        "writes": {"allowed": ["api.linear.app", "*.github.com"], "denied": []},
    }


def base_cells():
    return {"10.0.0.5": {"envIp": "10.0.0.5", "branchId": "feat-x"}}


def test_unknown_client_blocked():
    assert p.is_allowed("10.0.0.99", "api.linear.app", "GET", base_cells(), base_egress()) is False


def test_known_client_read_allowed():
    assert p.is_allowed("10.0.0.5", "api.example.com", "GET", base_cells(), base_egress()) is True


def test_v4_mapped_v6_known_client():
    assert p.is_allowed("::ffff:10.0.0.5", "api.example.com", "GET", base_cells(), base_egress()) is True


def test_write_allowed_listed_host():
    assert p.is_allowed("10.0.0.5", "api.linear.app", "POST", base_cells(), base_egress()) is True


def test_write_denied_unlisted_host():
    assert p.is_allowed("10.0.0.5", "evil.com", "POST", base_cells(), base_egress()) is False


def test_write_wildcard_match():
    assert p.is_allowed("10.0.0.5", "api.github.com", "PUT", base_cells(), base_egress()) is True


def test_specific_deny_wins_when_host_in_both_lists():
    egress = base_egress()
    egress["reads"] = {"methods": ["GET"], "allowed": ["safe.com"], "denied": ["safe.com"]}
    # explicit (non-wildcard) denial takes precedence — the safer semantic
    assert p.is_allowed("10.0.0.5", "safe.com", "GET", base_cells(), egress) is False


def test_wildcard_deny_is_overridden_by_allow():
    # default-deny floor + explicit allowlist (production write-policy shape)
    egress = base_egress()
    egress["writes"] = {"allowed": ["github.com"], "denied": "*"}
    assert p.is_allowed("10.0.0.5", "github.com", "POST", base_cells(), egress) is True
    assert p.is_allowed("10.0.0.5", "evil.com", "POST", base_cells(), egress) is False


def test_wildcard_deny_in_list_form_also_overridable():
    egress = base_egress()
    egress["writes"] = {"allowed": ["github.com"], "denied": ["*"]}
    assert p.is_allowed("10.0.0.5", "github.com", "POST", base_cells(), egress) is True
    assert p.is_allowed("10.0.0.5", "evil.com", "POST", base_cells(), egress) is False


def test_specific_deny_alongside_wildcard_still_wins():
    # denied=["*", "evil.com"]: wildcard is default-deny, but evil.com is
    # specific and must win even though github.com is allowed.
    egress = base_egress()
    egress["writes"] = {"allowed": ["github.com", "evil.com"], "denied": ["*", "evil.com"]}
    assert p.is_allowed("10.0.0.5", "github.com", "POST", base_cells(), egress) is True
    assert p.is_allowed("10.0.0.5", "evil.com", "POST", base_cells(), egress) is False


def test_domain_wildcard_in_deny_is_specific_not_default():
    # *.evil.com is a domain pattern, not the "everything" sentinel
    egress = base_egress()
    egress["writes"] = {"allowed": ["api.evil.com"], "denied": ["*.evil.com"]}
    assert p.is_allowed("10.0.0.5", "api.evil.com", "POST", base_cells(), egress) is False


def test_cell_additive_writes_allow_extra_host():
    envs = base_cells()
    envs["10.0.0.5"]["egress"] = {
        "additive": True,
        "writes": {"allowed": ["api.openai.com"]},
    }
    # env-level allowed REPLACES global allowed in additive mode
    assert p.is_allowed("10.0.0.5", "api.openai.com", "POST", envs, base_egress()) is True
    assert p.is_allowed("10.0.0.5", "api.linear.app", "POST", envs, base_egress()) is False


def test_cell_additive_denied_blocks_globally_allowed():
    envs = base_cells()
    envs["10.0.0.5"]["egress"] = {
        "additive": True,
        "writes": {"denied": ["api.linear.app"]},
    }
    # globally allowed but env denies it
    assert p.is_allowed("10.0.0.5", "api.linear.app", "POST", envs, base_egress()) is False


# ---- credential injection ---------------------------------------------------


def test_inject_global_cred_exact_host():
    creds = [{"host": "api.linear.app", "header": "Authorization", "envVar": "LIN"}]
    headers = p.compute_injected_headers("api.linear.app", creds, {"LIN": "tok"}, {})
    assert headers == {"Authorization": "Bearer tok"}


def test_inject_authorization_keeps_existing_bearer_prefix():
    creds = [{"host": "api.x.com", "header": "Authorization", "envVar": "K"}]
    headers = p.compute_injected_headers("api.x.com", creds, {"K": "Bearer abc"}, {})
    assert headers == {"Authorization": "Bearer abc"}


def test_inject_non_authorization_no_prefix():
    creds = [{"host": "api.anthropic.com", "header": "x-api-key", "envVar": "AK"}]
    headers = p.compute_injected_headers("api.anthropic.com", creds, {"AK": "raw-key"}, {})
    assert headers == {"x-api-key": "raw-key"}


def test_secrets_env_takes_precedence_over_environ():
    creds = [{"host": "x.com", "header": "x-api-key", "envVar": "K"}]
    headers = p.compute_injected_headers("x.com", creds, {"K": "from-secrets"}, {"K": "from-env"})
    assert headers == {"x-api-key": "from-secrets"}


def test_environ_used_when_secrets_missing():
    creds = [{"host": "x.com", "header": "x-api-key", "envVar": "K"}]
    headers = p.compute_injected_headers("x.com", creds, {}, {"K": "from-env"})
    assert headers == {"x-api-key": "from-env"}


def test_missing_env_var_skips_credential():
    creds = [{"host": "x.com", "header": "x-api-key", "envVar": "K"}]
    headers = p.compute_injected_headers("x.com", creds, {}, {})
    assert headers == {}


def test_inject_subdomain_match():
    creds = [{"host": "linear.app", "header": "Authorization", "envVar": "K"}]
    headers = p.compute_injected_headers("api.linear.app", creds, {"K": "tok"}, {})
    assert "Authorization" in headers


def test_inject_no_match_for_unrelated_host():
    creds = [{"host": "linear.app", "header": "Authorization", "envVar": "K"}]
    headers = p.compute_injected_headers("evil.com", creds, {"K": "tok"}, {})
    assert headers == {}


def test_inject_no_partial_match_for_lookalike():
    # host=api-linear.app should NOT match cred for linear.app
    creds = [{"host": "linear.app", "header": "Authorization", "envVar": "K"}]
    headers = p.compute_injected_headers("api-linear.app", creds, {"K": "tok"}, {})
    assert headers == {}


def test_inject_case_insensitive_host_match():
    creds = [{"host": "API.LINEAR.APP", "header": "Authorization", "envVar": "K"}]
    headers = p.compute_injected_headers("api.linear.app", creds, {"K": "tok"}, {})
    assert headers == {"Authorization": "Bearer tok"}


def test_collect_credentials_merges_global_and_cell():
    envs = {"10.0.0.5": {"egress": {"credentials": [{"host": "a.com", "header": "h", "envVar": "A"}]}}}
    creds = p.collect_credentials(envs, "10.0.0.5", [{"host": "b.com", "header": "h", "envVar": "B"}])
    hosts = {c["host"] for c in creds}
    assert hosts == {"a.com", "b.com"}


def test_collect_credentials_unknown_ip_only_global():
    envs = {}
    creds = p.collect_credentials(envs, "10.0.0.99", [{"host": "g.com", "header": "h", "envVar": "G"}])
    assert [c["host"] for c in creds] == ["g.com"]


# ---- envs indexing ---------------------------------------------------------


def test_index_envs_by_ip_basic():
    envs = [
        {"envIp": "10.0.0.5", "branchId": "a"},
        {"envIp": "10.0.0.6", "branchId": "b"},
    ]
    idx = p.index_envs_by_ip(envs)
    assert set(idx) == {"10.0.0.5", "10.0.0.6"}
    assert idx["10.0.0.5"]["branchId"] == "a"


def test_index_envs_by_ip_dedup_keeps_last():
    envs = [
        {"envIp": "10.0.0.5", "branchId": "old"},
        {"envIp": "10.0.0.5", "branchId": "new"},
    ]
    assert p.index_envs_by_ip(envs)["10.0.0.5"]["branchId"] == "new"


def test_index_envs_by_ip_skips_missing_ip():
    envs = [{"branchId": "no-ip"}, {"envIp": "", "branchId": "empty"}]
    assert p.index_envs_by_ip(envs) == {}


def test_index_envs_by_ip_handles_non_list():
    assert p.index_envs_by_ip(None) == {}
    assert p.index_envs_by_ip({"not": "a list"}) == {}


# ---- secrets.env parsing ----------------------------------------------------


def test_parse_secrets_env_basic():
    text = "FOO=bar\nBAZ=qux\n"
    assert p.parse_secrets_env(text) == {"FOO": "bar", "BAZ": "qux"}


def test_parse_secrets_env_skips_blanks_and_comments():
    text = "# comment\n\nFOO=bar\n   # indented comment\nBAZ=qux\n"
    assert p.parse_secrets_env(text) == {"FOO": "bar", "BAZ": "qux"}


def test_parse_secrets_env_value_with_equals():
    assert p.parse_secrets_env("URL=https://x.com/?a=1&b=2") == {"URL": "https://x.com/?a=1&b=2"}


def test_load_secrets_env_missing_file(tmp_path):
    assert p.load_secrets_env(tmp_path / "missing.env") == {}


def test_load_secrets_env_reads_file(tmp_path):
    f = tmp_path / "s.env"
    f.write_text("KEY=value\n")
    assert p.load_secrets_env(f) == {"KEY": "value"}


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-v"]))
