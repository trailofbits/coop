---
name: integration
description: Run and interpret coop's VM integration suite locally on Lima or remotely on Firecracker. Use when asked for integration testing or when guest-visible/lifecycle work needs its pre-merge gate.
---

# Integration

The runner is `./tests/run-integration.sh`. Empty arguments run locally on
macOS/Lima; `--remote user@host` cross-compiles and runs on Linux/Firecracker.
Other supported arguments include `--full`, `--profile LIST`, and `--name NAME`.

Confirm the requested platform and prerequisites. A single host covers only one
backend; never describe one backend as proving both. Run the suite with output
redirected to a file, narrate progress during the long run, inspect the complete
output, and report pass/fail per phase. Do not declare success unless the runner
exits zero. Quote the failing phase, clean up the output file, and state any
backend that was not run.
