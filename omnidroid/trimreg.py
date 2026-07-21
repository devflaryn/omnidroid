"""Register a trimmed base as a NEW version without disturbing the prior one.

A trim flatten produces a new image; the prior image MUST stay referenced-safe
because existing account overlays are COW-backed by it. This is pure cfg logic
(the live flatten is a runbook step); it mirrors the preserve-prior discipline
of test_dev_base_registration."""
import copy


def register_trim(cfg, base_key, new_disk, new_system, note):
    """Return a NEW cfg with base_key bumped a version, prior refs retained."""
    if base_key not in cfg.get("bases", {}):
        raise KeyError(f"no base {base_key!r}")
    out = copy.deepcopy(cfg)
    base = out["bases"][base_key]
    new_ver = int(base.get("version", 0)) + 1
    base["version"] = new_ver
    base["base_disk"] = new_disk
    base["system"] = new_system
    changelog = base.setdefault("changelog", {})
    changelog[str(new_ver)] = note
    return out
