# Decisions of this repository

Numbered from `0001`: `NNNN-short-title.md`, with a status, a date, the context, the decision and its
consequences. An accepted ADR is never rewritten: a later one supersedes it.

| ADR | Decision |
|---|---|
| [0001](0001-web-push.md) | Web Push through the browser vendors' push services, carrying a constant marker and never any content |
| [0002](0002-phone-push-relay.md) | Phones are woken through a relay the publisher runs, without content; a read on one device counts everywhere |

Both were decided in the former monolith (`Ruchoir/ruchoir`, its ADR 0001 and 0006), and moved here
on 2026-11-11.

Decisions that touch several repositories live in
[Ruchoir/.github](https://github.com/Ruchoir/.github/tree/main/docs/adr).
