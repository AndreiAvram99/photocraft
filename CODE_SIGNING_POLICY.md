# Code signing policy for Andrei Avram's PhotoCraft fork

This repository is a public fork of [storytold/photocraft](https://github.com/storytold/photocraft). Andrei Avram maintains the changes used by the [Remora software library](https://remora.cyliis.ro/learn/software), including the custom app icon and release packaging.

## Current status

Windows releases from this fork are **unsigned**. An application to the SignPath Foundation program is being prepared. Until a release explicitly states that it was signed and its signature verifies, do not treat the installer or executable as signed by SignPath Foundation.

## Proposed SignPath process

If the project is accepted, free code signing will be provided by [SignPath.io](https://signpath.io/), with a certificate issued to SignPath Foundation. Signing would apply only to artifacts built by GitHub Actions from this repository and approved for release. The source revision and published binaries will remain linked in each GitHub release. Every signing request will require manual approval.

Andrei Avram is the maintainer, code reviewer, and signing approver for this fork. Contributions from others will be reviewed before merging. Changes to build scripts and workflows receive the same review as application code.

## Privacy

The desktop editor processes documents locally. This fork does not add telemetry or a service that receives user documents. A network transfer may occur when the user explicitly invokes an integration or chooses a network destination.

Report a suspected signing or release problem through this repository's security policy or GitHub issues. If SignPath signing becomes active, this page and the release notes will be updated with the exact verification steps.
