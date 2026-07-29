# Security policy

## Reporting a vulnerability

Please report security issues privately using GitHub's **Report a vulnerability**
button under the repository's Security tab, which opens a private advisory.

Do not open a public issue for a vulnerability.

Please include the monitrs version, the OS and architecture, what an attacker
could achieve, and a reproduction if you have one. You will get an
acknowledgement within a few days; if you do not, assume the report was lost and
please follow up.

## What is in scope

monitrs is an unprivileged, read-mostly local program with no network access, so
its attack surface is narrow but not empty. These are in scope:

* **Unintended process signalling.** Any path where a signal reaches a process
  the user did not confirm — in particular any failure of the PID-reuse
  revalidation performed immediately before a signal is sent.
* **Signalling from a single keypress.** Any input sequence that sends a signal
  without an explicit confirmation step.
* **Process actions available while inspecting history.** These are supposed to
  be locked out entirely.
* **Secret disclosure.** Process command lines and environment values can contain
  credentials. Any path that writes them to a log, a JSON export, a crash
  report, or the terminal scrollback in a way the documentation says it will not.
* **Privilege escalation.** monitrs must never escalate, invoke `sudo`, or run
  any external command as part of an action.
* **Command execution from configuration.** Configuration is data. Any path that
  executes it is a vulnerability.
* **Unsafe code defects.** Memory-safety bugs in the platform collector FFI, or
  any unsafe block outside the approved collector modules.
* **Denial of service against the user's own terminal**, such as a state that
  leaves raw mode enabled and the terminal unusable after exit.
* **Dependency vulnerabilities** reachable from monitrs's own code paths.

## What is out of scope

* Reading system information the OS already grants to an unprivileged user.
  monitrs surfaces what `/proc`, `sysctl`, and friends already expose to you.
* Metrics being wrong, misleading, or differently defined from another tool.
  Those are correctness bugs — please use the "Incorrect metric" issue template.
* Anything requiring an attacker who can already execute code as your user, or
  already has root. Such an attacker does not need monitrs.
* Resource use on pathological systems, unless it is unbounded growth.
* Findings from automated scanners with no demonstrated impact.

## Supply chain

Releases are built by a pinned CI workflow. Every GitHub Action in a
release-sensitive workflow is pinned to an immutable commit SHA, `Cargo.lock` is
committed, and license and advisory checks (`cargo deny check`) run on every pull
request. Release archives ship SHA-256 checksums.

No publishing token is stored in the repository.

## Supported versions

Only the most recent release receives security fixes. monitrs is pre-`1.0`; there
are no long-term support branches.
