# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through GitHub:
[open a security advisory](https://github.com/syz51/agent-desktop/security/advisories/new).
Please don't open a public issue, pull request or discussion for them.

Include what an attacker can do, the affected commands or endpoints, the version or commit
(`desktop --version`), and steps to reproduce. Leave out real bearer tokens, Tailscale IPs,
hostnames and screenshots; placeholders are enough.

This is a one-maintainer project. Expect an acknowledgement within a week, and a fix or a
plan once the report is confirmed. You'll be credited in the advisory unless you ask not to be.

## Supported versions

Only the latest release, and `main`, get fixes while the project is pre-1.0.

## Scope

`desktopd` is designed for local `0600` Unix domain socket use and optional private Tailscale
fleet RPC (`100.64.0.0/10` with constant-time 256-bit bearer auth). Report a weakness even
when it requires local user access or Tailscale network access. Examples in scope:

- authentication or bind-restriction bypass on the remote HTTP RPC listener (including binding
  outside `100.64.0.0/10` / loopback when `require_tailscale = true`, or timing/comparison
  flaws in bearer token verification);
- unauthorized local access to the daemon Unix socket or managed `Xvfb` Xauthority cookie;
- bearer token or redacted password-field disclosure in logs, errors, audit trails, or snapshots;
- bypass of opt-in `deny_apps`, `confirm_apps`, `focus_mode = "background"`, or the physical
  stop/kill switch when enabled;
- command or shell injection in packaging scripts or Linux helper invocations (`xclip`, `grim`,
  `wtype`, `ydotool`, `open_app`).

Out of scope: intended owner-first control of local apps when `policy.mode = "open"` is active
with a valid local socket or bearer token, and deployments that expose the HTTP RPC port on a
public interface against the bind restrictions.
