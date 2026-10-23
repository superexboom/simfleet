# SPDX-License-Identifier: AGPL-3.0-or-later
import hashlib
import json


def validate_module(path, snapshots, entrypoint, wire=None):
    metadata = json.loads(path.with_suffix(".gpu.json").read_text())
    if hashlib.sha256(path.read_bytes()).hexdigest() != metadata["module_sha256"]:
        raise ValueError("GPU module checksum mismatch")
    if entrypoint not in metadata["entrypoints"]:
        raise ValueError("GPU module lacks requested entrypoint")
    if metadata.get("wire_sha256") and (wire is None or hashlib.sha256(wire.read_bytes()).hexdigest() != metadata["wire_sha256"]):
        raise ValueError("GPU native wire library mismatch")
    for snapshot in snapshots:
        for key in ("engine", "wasm_sha256", "wire_format"):
            if snapshot.get(key, 0 if key == "wire_format" else None) != metadata[key]:
                raise ValueError(f"GPU snapshot/module mismatch: {key}")
        if metadata.get("gas_address") is not None and snapshot.get("gas_address") != metadata["gas_address"]:
            raise ValueError("GPU gas address mismatch")
    return metadata
