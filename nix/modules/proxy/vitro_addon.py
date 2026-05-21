# /// script
# requires-python = ">=3.11"
# dependencies = ["mitmproxy"]
# ///
"""
Vitro mitmproxy addon — thin adapter over vitro_policy.

Reads config from:
  /etc/vitro/proxy-config.json  (static, from NixOS)
  /var/lib/vitro/envs.json      (dynamic, from control API)
"""

import json
import os
import time
from pathlib import Path

from mitmproxy import http, tls, ctx

import vitro_policy as policy


STATIC_CONFIG = Path("/etc/vitro/proxy-config.json")
DYNAMIC_ENVS = Path("/var/lib/vitro/envs.json")
SECRETS_ENV = Path("/var/lib/vitro/secrets.env")
LOG_FILE = Path("/var/log/vitro/proxy.log")
PER_ENV_LOG_DIR = Path("/var/log/vitro/per-env")


class VitroAddon:
    def __init__(self):
        self.egress: dict = {"reads": {}, "writes": {}}
        self.credentials: list[dict] = []
        self.passthrough: list[str] = []
        self._static_envs: dict[str, dict] = {}
        self._dynamic_envs: dict[str, dict] = {}
        self._envs_mtime = 0.0
        self._load_static()
        self._load_dynamic()

    @property
    def envs(self) -> dict[str, dict]:
        # dynamic (control API) wins on IP collision so that env-egress
        # overrides set at runtime take effect over the static config.
        return {**self._static_envs, **self._dynamic_envs}

    def _load_static(self):
        if not STATIC_CONFIG.exists():
            return
        try:
            cfg = json.loads(STATIC_CONFIG.read_text())
        except Exception as e:
            ctx.log.error(f"failed to load static config: {e}")
            return

        self.passthrough = cfg.get("egress", {}).get("passthrough", [])
        self.egress = cfg.get("egress", {"reads": {}, "writes": {}})
        self._static_envs = policy.index_envs_by_ip(cfg.get("envs", []))
        self.credentials = self.egress.get("credentials", [])

    def _load_dynamic(self):
        # dynamic file may be deleted/recreated; if missing, drop dynamic
        # envs entirely (static envs survive).
        if not DYNAMIC_ENVS.exists():
            if self._dynamic_envs:
                self._dynamic_envs = {}
                self._envs_mtime = 0.0
            return
        try:
            mtime = DYNAMIC_ENVS.stat().st_mtime
            if mtime <= self._envs_mtime:
                return
            self._envs_mtime = mtime
            envs_list = json.loads(DYNAMIC_ENVS.read_text())
            # full rebuild — handles registrations AND deregistrations
            self._dynamic_envs = policy.index_envs_by_ip(envs_list)
        except Exception:
            pass

    def _log(self, action: str, host: str, client: str, details: str = ""):
        ts = int(time.time())
        line = f"{ts} {action} {host} {client}"
        if details:
            line += f" {details}"
        ctx.log.info(line)
        try:
            with open(LOG_FILE, "a") as f:
                f.write(line + "\n")
        except Exception:
            pass

        # Operator-side per-env log: not mounted into the env (the agent must
        # not learn about credential-injection mechanics from log content).
        # Read via the laptop `vitro logs <env>` over SSH.
        env_name = (self.envs.get(client) or {}).get("envId")
        if env_name:
            try:
                PER_ENV_LOG_DIR.mkdir(parents=True, exist_ok=True)
                with open(PER_ENV_LOG_DIR / f"{env_name}.log", "a") as f:
                    f.write(line + "\n")
            except Exception:
                pass

    def tls_clienthello(self, data: tls.ClientHelloData):
        host = data.context.server.address[0] if data.context.server.address else ""
        if not host and data.client_hello:
            host = data.client_hello.sni or ""
        if policy.matches_any(host, self.passthrough):
            data.ignore_connection = True
            self._log("PASSTHROUGH", host, str(data.context.client.peername[0]))

    def request(self, flow: http.HTTPFlow):
        self._load_dynamic()

        client_ip = flow.client_conn.peername[0]
        host = flow.request.pretty_host
        method = flow.request.method
        path = flow.request.path

        if not policy.is_allowed(client_ip, host, method, self.envs, self.egress):
            self._log("BLOCKED", host, client_ip, f"{method} {path}")
            env_name = (self.envs.get(client_ip) or {}).get("envId", "?")
            direction = policy.classify_method(
                method,
                self.egress.get("reads", {}).get("methods") or policy.DEFAULT_READ_METHODS,
            )
            body = (
                f"Blocked by vitro proxy.\n"
                f"  env:    {env_name}\n"
                f"  host:   {host}\n"
                f"  method: {method} (classified as {direction})\n"
                f"  path:   {path}\n"
                f"\n"
                f"To allow this request, add {host!r} to "
                f"[egress].{direction}.allowed in .vitro/config.toml "
                f"and recreate the env.\n"
            ).encode()
            flow.response = http.Response.make(
                403, body, headers={"Content-Type": "text/plain; charset=utf-8"}
            )
            return

        direction = policy.classify_method(
            method,
            self.egress.get("reads", {}).get("methods") or policy.DEFAULT_READ_METHODS,
        )
        tag = "READ" if direction == "reads" else "WRITE"
        self._log(tag, host, client_ip, f"{method} {path}")

        creds = policy.collect_credentials(self.envs, client_ip, self.credentials)
        if creds:
            secrets = policy.load_secrets_env(SECRETS_ENV)
            headers = policy.compute_injected_headers(host, creds, secrets, os.environ)
            if not headers:
                # Either no cred matched this host, or the env var was empty —
                # both look like silent un-auth from the cell's side. Log it
                # so the failure mode (Pi creds dropped, 2026-05-11) is visible.
                self._log("NOAUTH", host, client_ip, f"{method} {path}")
            for k, v in headers.items():
                flow.request.headers[k] = v


addons = [VitroAddon()]
