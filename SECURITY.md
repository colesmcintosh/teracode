# Security policy

## Supported versions

TeraCode is pre-1.0. Security fixes are applied to the latest code on `main` and the latest `0.1.x` release when applicable.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting for this repository. Do not open a public issue for suspected command injection, sandbox escape, credential disclosure, unsafe process cleanup, worktree corruption, or another security-sensitive defect.

Include the affected version, platform, adapter, workspace/autonomy policies, reproduction steps, impact, and any suggested mitigation. Remove credentials and repository-sensitive content from logs before attaching them.

You should receive an acknowledgement through the private advisory within seven days. Coordinated disclosure is appreciated while a report is being validated and remediated.

## Scope

Particularly important boundaries include direct process construction, native provider policy mapping, environment/log redaction, process-group cancellation, Git worktree and patch assembly, persisted transcripts, and acceptance-command execution.
