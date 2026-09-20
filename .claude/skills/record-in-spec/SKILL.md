---
name: record-in-spec
description: Record new conventions, cross-cutting decisions, or behaviour changes in the Light Framework spec so they are normative and persistent. Use when a convention or rule is established or changed, when a design decision is made that outlives the conversation, or when framework behaviour changes in a way the spec should reflect (spec lives in light_mk5/documents).
---

# Record decisions and conventions in the spec

The specification under `light_mk5/documents/` is the persistent, normative home for the framework's
rules. When something is decided that should outlive this conversation, write it there — the rules
live in the repository, not in anyone's memory.

## What to record, and where

- **A new convention or cross-cutting rule** → the **Conventions** section of `00-overview.md`
  (documentation conventions vs normative code conventions).
- **A changed subsystem contract, behaviour, or invariant** → the relevant subsystem document
  (`01`–`10`), in the matching part of its format: responsibility / public surface / behaviour and
  invariants / notable design decisions and constraints.
- **A new subsystem** → a new document, added to the document-set table in `00-overview.md`.

## How to write it (authoring conventions)

Follow `00-overview.md` → Conventions → *Documentation conventions*:

- **Hardware-agnostic language.** Describe the general contract or capability; name a specific part
  only as a clearly-labelled reference/example implementation, with its part-specific facts confined
  to its own subsection. No board-model or vendor name-drops. Chip and port names belong only in
  `07-ports-and-shell.md`.
- **Keep the per-subsystem four-part format.**
- **Diagrams:** Mermaid in fenced blocks, each with a one-line italic caption; no raw `;` or `::` in
  labels; write angle brackets as `&lt;` / `&gt;`; solid arrows = a compile-time dependency, dashed =
  a runtime call.
- **Present tense, no history.** State what the framework *is*, never how it came to be. Record a
  decision as a rule with its reason beside it, not as a narrative of what changed; do not describe
  a superseded design. A change to existing behaviour rewrites the relevant text in place so the
  document always reads as the current definition.

## Keep code and spec in step

When you change framework behaviour, update the spec in the same change. When you record a decision,
make sure the code matches it (use the `refer-to-spec` skill).
