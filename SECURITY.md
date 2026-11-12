# Security policy

## Reporting a vulnerability

Do not open a public issue, pull request or discussion with an exploit,
proof-of-concept payload, pairing code, certificate or private key.

The preferred path is [GitHub private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
via the repository's **Security** tab ("Report a vulnerability"). That
feature is not enabled on this repository yet. Until it is, email
<windwerfer@gmail.com> (the maintainer address on recent commits) with a
short, non-exploitable description and wait for a private channel before
sending details or a sample.

A useful report includes the MirrorWire version or commit, operating system
and architecture, what is affected, the impact, and reproduction steps. Please
allow time to investigate before any public disclosure. This is a pre-1.0
project; there is no promised response deadline or supported-version window
beyond current `main`.

## What this project does

- **Static analysis.** [SonarQube Cloud](https://sonarcloud.io/project/overview?id=windwerfer_mirrorWire)
  is run from GitHub Actions on `main` and on pull requests from this
  repository (a Clippy report and unit-test lcov coverage are uploaded with
  the scan). Fork pull requests skip the upload when `SONAR_TOKEN` is
  unavailable. Automatic Analysis must stay off on the SonarCloud project,
  or the CI upload is rejected. [GitHub CodeQL](https://github.com/windwerfer/mirrorWire/security/code-scanning)
  is a second, separate scan of the Rust code and GitHub Actions workflows
  (`build-mode: none`) on `main`, pull requests, and a weekly schedule.
  Results appear under the repository's Security › Code scanning tab.
  CodeQL default setup must stay off, or the advanced workflow is rejected.
- **Dependency and licence checks.** `cargo audit` (RustSec) and `cargo deny`
  (advisories, bans, licences, sources) run on every push and pull request,
  and on a weekly schedule so new advisories against existing pins are
  caught. The Build workflow waits for that audit and will not produce
  artifacts if it fails.
- **CI supply chain.** Third-party GitHub Actions are pinned to full commit
  SHAs. Dependabot is configured to propose updates to those pins and to
  Cargo dependencies.
- **Packaging downloads.** Scripts that fetch FFmpeg, x264 and linuxdeploy
  use HTTPS (TLS 1.2 or newer) and check a pinned SHA-256 digest before
  unpacking or running the file.

## Security model

Each install generates a self-signed certificate once. The viewer and
presenter talk over QUIC with TLS 1.3. The viewer pins the presenter's
SHA-256 fingerprint (from the QR code, URI or mDNS record). The presenter
only streams to a viewer that has shown the six-digit pairing code once;
after that it remembers the viewer's certificate fingerprint. There is no
plaintext, "insecure", or skip-verification mode.

macOS builds are ad-hoc signed and not notarized. Gatekeeper will warn on
first launch; that is expected, not a silent security update channel.

This policy describes current practice. It is not a formal audit, a
guarantee, or a bug-bounty programme.
