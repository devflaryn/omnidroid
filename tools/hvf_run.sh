#!/bin/sh
# Cargo runner for macOS on Apple silicon: ad-hoc sign a test binary with the
# `com.apple.security.hypervisor` entitlement (tools/hvf.entitlements), then run it.
#
# Hypervisor.framework refuses `hv_vm_create` with HV_DENIED in an unsigned process, and the
# native CPU backend (`omni-cpu`'s `native-hvf` feature) reports that as a typed refusal rather
# than running. Used as
#
#   CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=$PWD/tools/hvf_run.sh cargo test ... --features native-hvf
#
# and never configured workspace-wide: a default build and test run is untouched.
set -e
binary="$1"
shift
codesign -s - -f --entitlements "$(dirname "$0")/hvf.entitlements" "$binary" 2>/dev/null
exec "$binary" "$@"
