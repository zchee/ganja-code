# ganja-code

**ganja** is a cloud-native AI coding agent that lives in your terminal: the
only agent process on your machine should be the one you are talking to.

Rust, a ratatui TUI, permission-gated tools, and a team model where agents
outlive the call that started them — including other vendors' CLIs (Claude Code,
Codex, Grok, and agy), driven as teammates.

> [!IMPORTANT]
> **ganja-code is under active development and not yet stable.** Features and
> configuration may change without notice.

## Design

Everything ganja fans out — subagents, teammates, whole parallel workstreams —
is headed for the cloud, with ganja as their control plane. Your terminal stays
the cockpit, and three things follow from that.

- **Your machine stops being the ceiling.** How large a fleet you run is no
  longer a question about your laptop's fans, its RAM, or how long its battery
  lasts.
- **Isolation is the default, not a setting.** An agent sandboxed in the cloud
  has no reach into your credentials, your SSH agent, or the rest of your home
  directory. The blast radius of a poisoned prompt or a bad tool call is
  whatever you sent it, rather than everything the process could touch — a
  distinction most of this field still treats as an afterthought.
- **Scale you cannot buy locally.** 10,000 subagents at once is a budget
  question rather than a hardware one.

> [!NOTE]
> None of the cloud behaviour above is implemented yet; it is the design this is
> being built toward. Teammates currently run on your own machine, in-process or
> in a tmux pane beside you, with the reach that implies.

If it looks useful, a **Star** is welcome. Releases are not published yet, so
**Watch** the repo to follow the work as it lands.
