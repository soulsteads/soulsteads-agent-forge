"""Regenerate tests/fixtures/demo-plugin/manifest.json (resource digests) from its files.

Run after editing any file under tests/fixtures/demo-plugin. A unit test fails when the
committed digests are stale.
"""
import hashlib
import json
from pathlib import Path

root = Path(__file__).resolve().parent / "demo-plugin"


def digest(relative):
    return "sha256:" + hashlib.sha256((root / relative).read_bytes()).hexdigest()


resources = [
    ("script", "main", "bin/demo-plugin"),
    ("job", "daily", "jobs/daily.json"),
    ("job", "slow", "jobs/slow.json"),
    ("workflow", "pipeline", "flows/pipeline.json"),
    ("asset", "page-js", "ui/page.js"),
    ("asset", "page-css", "ui/page.css"),
]
manifest = {
    "id": "demo-fixture",
    "version": "1.0.0",
    "api_version": "v2",
    "description": "Test-only fixture plugin with one contribution of every kind",
    "capabilities": ["fixture"],
    "requires": [],
    "conflicts": [],
    "runtime": {"executable": "script:main", "timeout_seconds": 20},
    "contributions": [
        {"kind": "job", "id": "daily", "metadata": {"description": "Scheduled demo job"}},
        {"kind": "job", "id": "slow", "metadata": {"description": "Long-running demo job"}},
        {"kind": "workflow", "id": "pipeline"},
        {"kind": "cli", "id": "hello", "metadata": {
            "command_path": ["hello"], "description": "Print a greeting",
            "disabled_denial": {"status": 3, "message": "demo-fixture is installed but disabled"}}},
        {"kind": "cli", "id": "fail", "metadata": {"command_path": ["fail"], "description": "Exit nonzero"}},
        {"kind": "api", "id": "web", "metadata": {"max_body_bytes": 1024, "routes": [
            {"method": "GET", "path": "/status", "capability": "read"},
            {"method": "POST", "path": "/echo"},
            {"method": "GET", "path": "/html"},
            {"method": "GET", "path": "/header-injection"}]}},
        {"kind": "mcp", "id": "echo", "metadata": {
            "tool_name": "echo", "description": "Echo text back", "read_only": True,
            "input_schema": {"type": "object", "properties": {"text": {"type": "string", "maxLength": 200}},
                             "required": ["text"], "additionalProperties": False}}},
        {"kind": "dashboard", "id": "page", "metadata": {"routes": [
            {"path": "home", "label": "Demo Plugin", "entry": "asset:page-js", "stylesheet": "asset:page-css"}]}},
        {"kind": "assets", "id": "static", "metadata": {"resources": ["asset:page-js", "asset:page-css"]}},
    ],
    "resource_declarations": [
        {"kind": kind, "id": ident, "path": path, "digest": digest(path)} for kind, ident, path in resources
    ],
}
(root / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print("wrote", root / "manifest.json")
