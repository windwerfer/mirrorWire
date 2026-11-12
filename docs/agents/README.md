# Agent and contributor docs

Start at [../../AGENTS.md](../../AGENTS.md) for the rules. These files are the maps.

| File | Contents |
|---|---|
| [decisions.md](decisions.md) | **Fixed decisions** (D1–D12) and the decision log. What must stay true and why. |
| [architecture.md](architecture.md) | Crates, layering and allowed dependency directions, the pipeline, threads, files on disk |
| [protocol.md](protocol.md) | Wire format, control messages, URI, mDNS record, versioning rules |
| [testing.md](testing.md) | Gates, loopback test, how to measure latency/quality, real-hardware testing, CI and artifacts |
| [debugging.md](debugging.md) | Checklist for "viewer finds presenter but can't connect", blurry picture, lag; stats overlay fields |
| [roadmap.md](roadmap.md) | Next work, open questions, known debt and doc/code mismatches |

Keep these short and true. When code and a doc disagree, fix the one that is wrong in the same
commit as the change that exposed it, or list it under "Known debt" in [roadmap.md](roadmap.md).
