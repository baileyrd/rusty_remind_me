# Architecture

## Overview
A Rust-based reimplementation of
[baileyrd/Daily-Backup-System](https://github.com/baileyrd/Daily-Backup-System).
No code has landed yet — this file will fill in for real (boundaries, data flow)
once there's an actual first slice to describe rather than a plan.

## Boundaries
<!-- Domain logic vs. I/O and framework details (ports-and-adapters).
     List the ports (interfaces) and the adapters that implement them. -->

| Port | Adapter(s) | Notes |
| ---- | ---------- | ----- |
|      |            |       |

## Structure
<!-- Greenfield default (see references/scan-and-defaults.md): modular monolith,
     composition over inheritance, ports-and-adapters keeping domain logic free of
     I/O and framework details. A component gets split into its own service only for
     a concrete forcing function — independent scaling, a team/language boundary, or
     hard fault isolation. Note here if/why this repo has already crossed that line. -->

## Data flow
<!-- Diagram or short walkthrough of a request/event through the system -->

## Key decisions
See [docs/adr/](./docs/adr/) for the record of individual decisions and their tradeoffs.

## Non-goals
<!-- Explicitly out of scope, and why -->
