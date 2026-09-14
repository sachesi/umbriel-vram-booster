# Security

Please report a vulnerability privately rather than in a public issue: through
[a private advisory](https://github.com/sachesi/umbriel-vram-booster/security/advisories/new)
on GitHub, or by mail to sachesi <xsachesi@pm.me>. Say what you found, how to reproduce it
and which version you ran; a fix is worked out with you before anything is published.

Only the latest release gets fixes.

## What counts

The daemon runs as the session user and writes cgroup files on the strength of what other
programs tell it. The parts where a mistake matters most:

- Where it writes. It must write only `dmem.low`, and only below its own
  `user-<uid>.slice`; a way to make it write another file, another user's cgroup, or a
  value other than its boost or zero is a vulnerability.
- What it reads from others. Window app ids and pids come from Umbriel on behalf of any
  client, and unit names, `comm` and process environments from any process in the session.
  A way to forge log lines through them, to make a lookup run unbounded, or to make the
  daemon crash or stop following focus is a vulnerability.
- The D-Bus service `org.umbriel.VramBooster` on the session bus, and the hardening of the
  systemd unit in `data/`.

A window that is boosted when it should not be, or not boosted when it should, is a bug;
please file it as an ordinary issue.
