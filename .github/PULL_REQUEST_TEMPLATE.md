<!--
Base branch: `dev`, not `main`.

GitHub pre-fills the base with this repo's default branch (`main`), the
release line. Use the "Edit" button next to the title to switch the base
to `dev`; with the CLI, pass `--base dev`.

Before writing: this repository is a set of crates, not an application.
The desktop app lives in ThinkWatch-Lite. See CONTRIBUTING.md.
-->

## What this changes

<!-- One or two sentences. What behavior is different after this merges? -->

## Why

<!-- The problem, not the patch. If it fixes an issue, link it. -->

## How it was verified

<!--
What you ran and what it printed. "cargo test passes" is weaker than
"the 3 new tests in crates/tw-yaml cover the flow-style insert; 1057
tests and 41 smoke checks green".

This crate set sits on the data path and handles every key the user
owns. If your change touches routing, forwarding, redaction, masking, or
anything that reads a credential, say how you convinced yourself it
doesn't leak one or weaken an existing check.
-->

## Notes for review

<!-- Anything you're unsure about, deliberately left out, or want argued with. -->
