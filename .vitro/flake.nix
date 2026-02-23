{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { nixpkgs, ... }: {
    nixosModule = { pkgs, lib, ... }: {
      # Claude Code config — only applies inside this env. Bypasses
      # per-tool permission prompts since the vitro sandbox is the
      # actual security boundary. Rewritten on every boot so the
      # declarative config is authoritative across env rebuilds.
      systemd.services.claude-settings = {
        description = "Install per-env Claude Code settings";
        wantedBy = [ "multi-user.target" ];
        after = [ "local-fs.target" ];
        serviceConfig.Type = "oneshot";
        script = let
          settings = builtins.toJSON {
            permissions = {
              defaultMode = "bypassPermissions";
              skipDangerousModePermissionPrompt = true;
            };
          };
        in ''
          mkdir -p /home/agent/.claude
          echo ${lib.escapeShellArg settings} > /home/agent/.claude/settings.json
          chown -R agent:users /home/agent/.claude
          chmod 600 /home/agent/.claude/settings.json
        '';
      };
    };
  };
}
