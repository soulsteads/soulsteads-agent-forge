#!/usr/bin/env python3
"""Pinned GitHub release-readiness collector using Forge stdio plugin protocol."""
from __future__ import annotations

import hashlib
import http.client
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

FORMAT = "forge-pinned-release-readiness/v1"
RECEIPT_FORMAT = "forge-release-readiness-receipt/v1"
API_ROOT = "https://api.github.com"
ALLOWLIST_ENV = "RELEASE_READINESS_ALLOWED_REPOSITORY"
FIXTURE_REPOSITORY = "forge-fixture/release-readiness"
FIXTURE_IDS = {
    "ready", "pending", "failed", "no-tag", "no-assets", "unavailable",
    "partial", "mismatched", "tag-mismatched", "unverified",
}
MAX_INPUT_BYTES = 256 * 1024
MAX_RESPONSE_BYTES = 1024 * 1024
MAX_PAGES = 10
PAGE_SIZE = 100
MAX_TAG_DEPTH = 8
TOTAL_DEADLINE_SECONDS = 12.0
REQUEST_TIMEOUT_SECONDS = 3.0
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
REPOSITORY_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
NAME_RE = re.compile(r"^[^\x00-\x1f\x7f]{1,200}$")


class InputError(Exception):
    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


class ApiUnavailable(Exception):
    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


class InvalidEvidence(Exception):
    pass


def _is_sha(value: object) -> bool:
    return isinstance(value, str) and SHA_RE.fullmatch(value) is not None


def _valid_names(value: object) -> bool:
    return (
        isinstance(value, list)
        and bool(value)
        and all(isinstance(name, str) and NAME_RE.fullmatch(name) for name in value)
        and len(set(value)) == len(value)
    )


def _valid_timestamp(value: object) -> bool:
    if not isinstance(value, str) or not value.endswith("Z"):
        return False
    try:
        parsed = datetime.fromisoformat(value[:-1] + "+00:00")
    except ValueError:
        return False
    return parsed.utcoffset() == timezone.utc.utcoffset(parsed)


def _validate_input(value: object) -> dict:
    if not isinstance(value, dict):
        raise InputError("input_must_be_object")
    allowed = {
        "mode", "fixture_id", "repository", "revision", "tag", "required_checks",
        "required_artifacts", "observed_at",
    }
    if set(value) - allowed:
        raise InputError("unexpected_input_field")
    mode = value.get("mode")
    repository = value.get("repository")
    revision = value.get("revision")
    tag = value.get("tag")
    if mode not in ("fixture", "live"):
        raise InputError("invalid_mode")
    if not isinstance(repository, str) or not REPOSITORY_RE.fullmatch(repository):
        raise InputError("invalid_repository")
    if not _is_sha(revision):
        raise InputError("revision_must_be_full_40_character_sha")
    if not isinstance(tag, str) or not NAME_RE.fullmatch(tag):
        raise InputError("invalid_tag")
    checks = value.get("required_checks")
    artifacts = value.get("required_artifacts")
    if not isinstance(checks, list) or not _valid_names(checks):
        raise InputError("invalid_required_checks")
    if not isinstance(artifacts, list) or not _valid_names(artifacts):
        raise InputError("invalid_required_artifacts")
    if mode == "live" and "observed_at" in value:
        raise InputError("live_observed_at_not_allowed")
    observed_at = value.get("observed_at")
    if observed_at is not None and not _valid_timestamp(observed_at):
        raise InputError("observed_at_must_be_utc_rfc3339")
    if mode == "fixture":
        if repository != FIXTURE_REPOSITORY:
            raise InputError("fixture_repository_must_be_synthetic_fixture")
        fixture_id = value.get("fixture_id")
        if not isinstance(fixture_id, str) or fixture_id not in FIXTURE_IDS:
            raise InputError("unknown_fixture_id")
        if observed_at is None:
            raise InputError("fixture_observed_at_required")
    else:
        if "fixture_id" in value:
            raise InputError("fixture_id_not_allowed_in_live_mode")
        configured = os.environ.get(ALLOWLIST_ENV, "")
        if not configured or not REPOSITORY_RE.fullmatch(configured):
            raise InputError("live_repository_allowlist_not_configured")
        if repository != configured:
            raise InputError("repository_not_allowlisted")
    normalized = dict(value)
    normalized["required_checks"] = list(checks)
    normalized["required_artifacts"] = list(artifacts)
    if observed_at is None:
        normalized["observed_at"] = (
            datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
        )
    return normalized


def _fixture_snapshot(fixture_id: str) -> dict:
    path = Path(__file__).resolve().parent / "fixtures" / "samples.json"
    try:
        contents = json.loads(path.read_text(encoding="utf-8"))
        if contents.get("format") != "github-release-readiness-fixtures/v1":
            raise InvalidEvidence("fixture_format")
        snapshot = contents["scenarios"][fixture_id]
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise InvalidEvidence("fixture_unavailable") from exc
    if not isinstance(snapshot, dict):
        raise InvalidEvidence("fixture_shape")
    return snapshot


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class GitHubReader:
    """Read a single allowlisted public repository via fixed GitHub API paths."""

    def __init__(self, repository: str):
        self.repository = repository
        self.deadline = time.monotonic() + TOTAL_DEADLINE_SECONDS
        self.opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}), _NoRedirect
        )
        self.owner, self.name = repository.split("/", 1)
        self.prefix = f"/repos/{self.owner}/{self.name}"
        self.request_count = 0

    def _read_response_body(self, response) -> bytes:
        body = bytearray()
        while len(body) <= MAX_RESPONSE_BYTES:
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise ApiUnavailable("overall_deadline")
            raw = getattr(getattr(response, "fp", None), "raw", None)
            sock = getattr(raw, "_sock", None)
            try:
                if sock is not None:
                    sock.settimeout(min(REQUEST_TIMEOUT_SECONDS, remaining))
                chunk = response.read(min(64 * 1024, MAX_RESPONSE_BYTES + 1 - len(body)))
            except http.client.IncompleteRead:
                raise ApiUnavailable("incomplete_response") from None
            except http.client.HTTPException:
                raise ApiUnavailable("invalid_http_response") from None
            except (TimeoutError, OSError, urllib.error.URLError) as exc:
                reason = "timeout" if isinstance(exc, TimeoutError) else "network_error"
                raise ApiUnavailable(reason) from None
            if time.monotonic() >= self.deadline:
                raise ApiUnavailable("overall_deadline")
            if not chunk:
                if getattr(response, "length", None) not in (None, 0):
                    raise ApiUnavailable("incomplete_response")
                break
            body.extend(chunk)
        return bytes(body)

    def get_json(self, path: str, *, not_found_is_data: bool = False):
        # All callers construct fixed API paths; never accept a URL or redirect target.
        if not path.startswith(self.prefix + "/") or path.startswith("//"):
            raise InvalidEvidence("api_path_outside_repository")
        if self.request_count >= MAX_PAGES * 3 + MAX_TAG_DEPTH + 4:
            raise ApiUnavailable("request_limit")
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise ApiUnavailable("overall_deadline")
        self.request_count += 1
        url = API_ROOT + path
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "forge-pinned-release-readiness-example/1",
            },
            method="GET",
        )
        try:
            response = self.opener.open(
                request, timeout=min(REQUEST_TIMEOUT_SECONDS, remaining)
            )
        except urllib.error.HTTPError as exc:
            if exc.code == 404 and not_found_is_data:
                return None
            raise ApiUnavailable(f"http_{exc.code}") from None
        except (TimeoutError, OSError, urllib.error.URLError) as exc:
            reason = "timeout" if isinstance(exc, TimeoutError) else "network_error"
            raise ApiUnavailable(reason) from None
        with response:
            if response.geturl() != url:
                raise InvalidEvidence("unexpected_response_url")
            body = self._read_response_body(response)
        if len(body) > MAX_RESPONSE_BYTES:
            raise ApiUnavailable("response_too_large")
        try:
            return json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            raise InvalidEvidence("invalid_json_response") from None

    def read_checks(self, revision: str) -> dict:
        runs = []
        paths = []
        total_count = None
        for page in range(1, MAX_PAGES + 1):
            path = f"{self.prefix}/commits/{revision}/check-runs?per_page={PAGE_SIZE}&page={page}"
            data = self.get_json(path)
            paths.append(path)
            if not isinstance(data, dict) or not isinstance(data.get("check_runs"), list):
                raise InvalidEvidence("invalid_check_runs_response")
            count = data.get("total_count")
            items = data["check_runs"]
            if isinstance(count, bool) or not isinstance(count, int) or count < 0:
                raise InvalidEvidence("invalid_check_runs_count")
            if total_count is None:
                total_count = count
            elif count != total_count:
                return {"state": "partial", "check_runs": runs + items, "total_count": count, "api_paths": paths}
            if len(items) > PAGE_SIZE:
                raise InvalidEvidence("oversized_check_runs_page")
            runs.extend(items)
            if len(runs) >= total_count:
                if len(runs) != total_count:
                    return {"state": "partial", "check_runs": runs, "total_count": total_count, "api_paths": paths}
                return {"state": "complete", "check_runs": runs, "total_count": total_count, "api_paths": paths}
            if len(items) < PAGE_SIZE:
                return {"state": "partial", "check_runs": runs, "total_count": total_count, "api_paths": paths}
        return {"state": "partial", "check_runs": runs, "total_count": total_count, "api_paths": paths}

    def read_tag(self, tag: str) -> dict:
        encoded_tag = urllib.parse.quote(tag, safe="")
        path = f"{self.prefix}/git/ref/tags/{encoded_tag}"
        data = self.get_json(path, not_found_is_data=True)
        if data is None:
            return {"state": "absent", "api_paths": [path]}
        expected_ref = f"refs/tags/{tag}"
        if not isinstance(data, dict) or data.get("ref") != expected_ref:
            raise InvalidEvidence("tag_ref_mismatch")
        current = data.get("object")
        if not isinstance(current, dict):
            raise InvalidEvidence("invalid_tag_ref_object")
        objects = []
        paths = [path]
        seen = set()
        for depth in range(MAX_TAG_DEPTH + 1):
            if not isinstance(current, dict):
                raise InvalidEvidence("invalid_tag_object")
            sha, kind = current.get("sha"), current.get("type")
            if not _is_sha(sha) or sha in seen or kind not in ("commit", "tag"):
                raise InvalidEvidence("unverified_tag_object")
            seen.add(sha)
            objects.append({"sha": sha, "type": kind})
            if kind == "commit":
                return {"state": "present", "reference": expected_ref, "objects": objects, "api_paths": paths}
            if depth == MAX_TAG_DEPTH:
                raise InvalidEvidence("tag_depth_limit")
            tag_path = f"{self.prefix}/git/tags/{sha}"
            details = self.get_json(tag_path)
            paths.append(tag_path)
            if (
                not isinstance(details, dict)
                or details.get("sha") != sha
                or not isinstance(details.get("object"), dict)
            ):
                raise InvalidEvidence("invalid_annotated_tag")
            if depth == 0 and details.get("tag") != tag:
                raise InvalidEvidence("annotated_tag_name_mismatch")
            current = details["object"]
        raise InvalidEvidence("tag_resolution_failed")

    def read_assets(self, release_id: int) -> dict:
        assets = []
        paths = []
        for page in range(1, MAX_PAGES + 1):
            path = f"{self.prefix}/releases/{release_id}/assets?per_page={PAGE_SIZE}&page={page}"
            data = self.get_json(path)
            paths.append(path)
            if not isinstance(data, list) or len(data) > PAGE_SIZE:
                raise InvalidEvidence("invalid_release_assets_response")
            assets.extend(data)
            if len(data) < PAGE_SIZE:
                return {"state": "complete", "assets": assets, "api_paths": paths}
        return {"state": "partial", "assets": assets, "api_paths": paths}

    def read_release(self, tag: str) -> dict:
        encoded_tag = urllib.parse.quote(tag, safe="")
        path = f"{self.prefix}/releases/tags/{encoded_tag}"
        data = self.get_json(path, not_found_is_data=True)
        if data is None:
            return {"state": "absent", "api_paths": [path]}
        if not isinstance(data, dict):
            raise InvalidEvidence("invalid_release_response")
        release_id = data.get("id")
        if isinstance(release_id, bool) or not isinstance(release_id, int) or release_id <= 0:
            raise InvalidEvidence("invalid_release_id")
        assets = self.read_assets(release_id)
        return {
            "state": "present", "id": release_id, "tag_name": data.get("tag_name"),
            "assets_state": assets["state"], "assets": assets["assets"],
            "api_paths": [path], "asset_api_paths": assets["api_paths"],
        }

    def snapshot(self, revision: str, tag: str) -> dict:
        snapshot = {}
        readers = (
            ("checks", lambda: self.read_checks(revision)),
            ("tag", lambda: self.read_tag(tag)),
            ("release", lambda: self.read_release(tag)),
        )
        for name, reader in readers:
            try:
                snapshot[name] = reader()
            except ApiUnavailable as exc:
                snapshot[name] = {"state": "unavailable", "error": exc.code}
            except InvalidEvidence as exc:
                snapshot[name] = {"state": "unverified", "error": str(exc)}
        return snapshot


def _api_path(snapshot: dict, section: str, fallback: str) -> str:
    source = snapshot.get(section, {})
    paths = source.get("api_paths", []) if isinstance(source, dict) else []
    return paths[0] if isinstance(paths, list) and paths and isinstance(paths[0], str) else fallback


def _build_report(value: dict, snapshot: dict) -> dict:
    repository = value["repository"]
    revision = value["revision"]
    tag_name = value["tag"]
    required_checks = value["required_checks"]
    required_artifacts = value["required_artifacts"]
    evidence = []
    incomplete_sources = []
    source_states = []

    checks_source = snapshot.get("checks", {})
    checks_state = checks_source.get("state") if isinstance(checks_source, dict) else None
    if checks_state not in ("complete", "partial", "unavailable", "unverified"):
        checks_state = "unverified"
    source_states.append(checks_state)
    if checks_state != "complete":
        incomplete_sources.append("checks")
    check_paths = checks_source.get("api_paths", []) if isinstance(checks_source, dict) else []
    if not isinstance(check_paths, list) or not check_paths:
        check_paths = [f"/repos/{repository}/commits/{revision}/check-runs"]
    check_paths = [path for path in check_paths if isinstance(path, str)]
    check_path = check_paths[0] if check_paths else f"/repos/{repository}/commits/{revision}/check-runs"
    if not check_paths:
        check_paths = [check_path]
    check_outcomes = {"passed": [], "pending": [], "failed": [], "missing": [], "unverified": []}
    check_runs = checks_source.get("check_runs", []) if isinstance(checks_source, dict) else []
    if not isinstance(check_runs, list):
        check_runs = []
        checks_state = "unverified"
        incomplete_sources.append("checks")
    for required in required_checks:
        matches = [run for run in check_runs if isinstance(run, dict) and run.get("name") == required]
        if not matches:
            if checks_state == "complete":
                check_outcomes["missing"].append(required)
            continue
        if len(matches) != 1:
            check_outcomes["unverified"].append(required)
            incomplete_sources.append("checks")
            continue
        run = matches[0]
        observed_sha = run.get("head_sha")
        run_id = run.get("id")
        ref = {
            "kind": "github_check_run", "repository": repository, "revision": revision,
            "observed_revision": observed_sha, "name": required,
            "api_path": check_path, "api_paths": check_paths,
            "verified": False,
        }
        if isinstance(run_id, int) and not isinstance(run_id, bool) and run_id > 0:
            ref["check_run_id"] = run_id
        if not _is_sha(observed_sha) or observed_sha != revision or not (isinstance(run_id, int) and not isinstance(run_id, bool) and run_id > 0):
            check_outcomes["unverified"].append(required)
            evidence.append(ref)
            incomplete_sources.append("checks")
            continue
        status = run.get("status")
        conclusion = run.get("conclusion")
        ref["status"] = status if isinstance(status, str) else None
        ref["conclusion"] = conclusion if isinstance(conclusion, str) else None
        ref["verified"] = True
        evidence.append(ref)
        if status == "completed" and conclusion == "success":
            check_outcomes["passed"].append(required)
        elif status == "completed" and isinstance(conclusion, str):
            check_outcomes["failed"].append(required)
        elif status in ("queued", "in_progress", "requested", "waiting", "pending") and conclusion is None:
            check_outcomes["pending"].append(required)
        else:
            check_outcomes["unverified"].append(required)
            incomplete_sources.append("checks")
            ref["verified"] = False

    tag_source = snapshot.get("tag", {})
    tag_source_state = tag_source.get("state") if isinstance(tag_source, dict) else None
    if tag_source_state not in ("present", "absent", "unavailable", "unverified"):
        tag_source_state = "unverified"
    source_states.append(tag_source_state)
    if tag_source_state in ("unavailable", "unverified"):
        incomplete_sources.append("tag")
    tag_state = tag_source_state
    tag_target = None
    tag_paths = tag_source.get("api_paths", []) if isinstance(tag_source, dict) else []
    tag_path = tag_paths[0] if isinstance(tag_paths, list) and tag_paths and isinstance(tag_paths[0], str) else f"/repos/{repository}/git/ref/tags/{urllib.parse.quote(tag_name, safe='')}"
    tag_ref = {
        "kind": "github_tag_ref", "repository": repository, "revision": revision,
        "tag": tag_name, "api_path": tag_path,
        "api_paths": tag_paths if isinstance(tag_paths, list) and tag_paths else [tag_path],
        "verified": False,
    }
    if tag_source_state == "present":
        objects = tag_source.get("objects", [])
        reference = tag_source.get("reference")
        object_chain_valid = (
            isinstance(objects, list) and 1 <= len(objects) <= MAX_TAG_DEPTH + 1
            and reference == f"refs/tags/{tag_name}"
        )
        seen = set()
        if object_chain_valid:
            for index, item in enumerate(objects):
                if not isinstance(item, dict) or not _is_sha(item.get("sha")) or item["sha"] in seen:
                    object_chain_valid = False
                    break
                seen.add(item["sha"])
                expected_kind = "commit" if index == len(objects) - 1 else "tag"
                if item.get("type") != expected_kind:
                    object_chain_valid = False
                    break
        if not object_chain_valid:
            tag_state = "unverified"
            incomplete_sources.append("tag")
        else:
            tag_target = objects[-1]["sha"]
            tag_ref["target_revision"] = tag_target
            tag_ref["object_chain"] = [item["sha"] for item in objects]
            if not isinstance(tag_paths, list) or not tag_paths:
                tag_ref["api_paths"] = [tag_path] + [
                    f"/repos/{repository}/git/tags/{item['sha']}"
                    for item in objects[:-1]
                ]
            tag_ref["verified"] = tag_target == revision
            if tag_target != revision:
                tag_state = "mismatch"
                incomplete_sources.append("tag")
            else:
                tag_state = "present"
    elif tag_source_state == "absent":
        tag_ref["verified"] = True
    evidence.append(tag_ref)

    release_source = snapshot.get("release", {})
    release_state = release_source.get("state") if isinstance(release_source, dict) else None
    if release_state not in ("present", "absent", "unavailable", "unverified"):
        release_state = "unverified"
    source_states.append(release_state)
    if release_state in ("unavailable", "unverified"):
        incomplete_sources.append("release")
    assets_state = release_source.get("assets_state") if isinstance(release_source, dict) else None
    assets = release_source.get("assets", []) if isinstance(release_source, dict) else []
    if release_state == "present" and assets_state not in ("complete", "partial", "unavailable", "unverified"):
        assets_state = "unverified"
    if release_state == "present":
        source_states.append(assets_state)
        if assets_state != "complete":
            incomplete_sources.append("release_assets")
    release_paths = release_source.get("api_paths", []) if isinstance(release_source, dict) else []
    release_path = release_paths[0] if isinstance(release_paths, list) and release_paths and isinstance(release_paths[0], str) else f"/repos/{repository}/releases/tags/{urllib.parse.quote(tag_name, safe='')}"
    release_ref = {
        "kind": "github_release", "repository": repository, "revision": revision,
        "tag": tag_name, "api_path": release_path,
        "api_paths": [path for path in release_paths if isinstance(path, str)] if isinstance(release_paths, list) else [release_path],
        "verified": False,
    }
    if not release_ref["api_paths"]:
        release_ref["api_paths"] = [release_path]
    assets_by_name = {}
    if release_state == "absent":
        release_ref["verified"] = True
    elif release_state == "present":
        release_id = release_source.get("id")
        if isinstance(release_id, int) and not isinstance(release_id, bool) and release_id > 0:
            release_ref["release_id"] = release_id
        if release_source.get("tag_name") != tag_name or not (isinstance(release_id, int) and not isinstance(release_id, bool) and release_id > 0):
            release_state = "mismatch"
            incomplete_sources.append("release")
        else:
            release_ref["verified"] = tag_state == "present"
            if tag_state != "present":
                incomplete_sources.append("release")
            asset_paths = release_source.get("asset_api_paths", [])
            asset_path = asset_paths[0] if isinstance(asset_paths, list) and asset_paths and isinstance(asset_paths[0], str) else f"/repos/{repository}/releases/{release_id}/assets?per_page={PAGE_SIZE}&page=1"
            release_ref["assets_api_path"] = asset_path
            for observed_path in asset_paths if isinstance(asset_paths, list) and asset_paths else [asset_path]:
                if isinstance(observed_path, str) and observed_path not in release_ref["api_paths"]:
                    release_ref["api_paths"].append(observed_path)
            if not isinstance(assets, list):
                assets_state = "unverified"
                assets = []
                incomplete_sources.append("release_assets")
            for asset in assets:
                if not isinstance(asset, dict):
                    incomplete_sources.append("release_assets")
                    continue
                name = asset.get("name")
                asset_id = asset.get("id")
                size = asset.get("size")
                uploaded = asset.get("state") == "uploaded"
                verified_asset = (
                    isinstance(name, str) and bool(NAME_RE.fullmatch(name))
                    and isinstance(asset_id, int) and not isinstance(asset_id, bool) and asset_id > 0
                    and isinstance(size, int) and not isinstance(size, bool) and size > 0
                    and uploaded
                )
                if not isinstance(name, str):
                    incomplete_sources.append("release_assets")
                    continue
                assets_by_name.setdefault(name, []).append((asset, verified_asset))
                if not verified_asset:
                    incomplete_sources.append("release_assets")

    evidence.append(release_ref)
    artifact_outcomes = {"present": [], "missing": [], "unverified": []}
    for required in required_artifacts:
        matches = assets_by_name.get(required, [])
        if len(matches) == 1 and matches[0][1] and release_ref["verified"]:
            asset = matches[0][0]
            artifact_outcomes["present"].append(required)
            evidence.append({
                "kind": "github_release_asset", "repository": repository,
                "revision": revision, "tag": tag_name, "name": required,
                "release_id": release_ref.get("release_id"), "asset_id": asset["id"],
                "size": asset["size"], "state": asset["state"],
                "api_path": release_ref.get("assets_api_path", release_path),
                "verified": True,
            })
        elif len(matches) > 1 or matches and not matches[0][1]:
            artifact_outcomes["unverified"].append(required)
            incomplete_sources.append("release_assets")
        elif release_state == "absent" or (release_state == "present" and assets_state == "complete" and release_ref["verified"]):
            artifact_outcomes["missing"].append(required)
        else:
            artifact_outcomes["unverified"].append(required)
            incomplete_sources.append("release_assets")

    effective_states = [state for state in source_states if state is not None]
    if effective_states and all(state == "unavailable" for state in effective_states):
        collection_status = "unavailable"
    elif incomplete_sources or any(state in ("partial", "unavailable", "unverified") for state in effective_states):
        collection_status = "partial"
    else:
        collection_status = "complete"

    checks_satisfied = (
        not check_outcomes["failed"] and not check_outcomes["missing"]
        and not check_outcomes["pending"] and not check_outcomes["unverified"]
    )
    if checks_state != "complete":
        checks_satisfied = None
    elif check_outcomes["pending"] or check_outcomes["unverified"]:
        checks_satisfied = None
    elif check_outcomes["failed"] or check_outcomes["missing"]:
        checks_satisfied = False
    tag_satisfied = True if tag_state == "present" else (False if tag_state == "absent" else None)
    if release_state == "absent" and assets_state is None:
        for name in required_artifacts:
            if name not in artifact_outcomes["missing"]:
                artifact_outcomes["missing"].append(name)
    artifacts_satisfied = (
        not artifact_outcomes["missing"] and not artifact_outcomes["unverified"]
        and len(artifact_outcomes["present"]) == len(required_artifacts)
    )
    if release_state not in ("present", "absent") or assets_state not in (None, "complete") or artifact_outcomes["unverified"]:
        artifacts_satisfied = None
    elif artifact_outcomes["missing"]:
        artifacts_satisfied = False

    if collection_status != "complete":
        verdict = "incomplete"
    elif check_outcomes["failed"] or check_outcomes["missing"] or tag_state == "absent" or artifact_outcomes["missing"]:
        verdict = "not_ready"
    elif check_outcomes["pending"] or check_outcomes["unverified"] or tag_state != "present" or artifact_outcomes["unverified"]:
        verdict = "incomplete"
    else:
        verdict = "ready"

    return {
        "format": FORMAT,
        "repository": repository,
        "revision": revision,
        "tag": {"name": tag_name, "state": tag_state, "target_revision": tag_target},
        "required_checks": required_checks,
        "artifact_policy": {
            "required_names": required_artifacts,
            "match": "exact GitHub release asset name",
            "required_asset_state": "uploaded",
            "minimum_size_bytes": 1,
            "content_bytes_downloaded": False,
        },
        "observed_at": value["observed_at"],
        "criteria": {
            "checks": {"required": required_checks, **check_outcomes, "satisfied": checks_satisfied},
            "tag": {"required": True, "state": tag_state, "satisfied": tag_satisfied},
            "artifacts": {"required": required_artifacts, **artifact_outcomes, "satisfied": artifacts_satisfied},
        },
        "evidence_references": evidence,
        "collection_status": collection_status,
        "verdict": verdict,
    }


def _envelope(report: dict) -> dict:
    report_bytes = json.dumps(
        report, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")
    return {
        "format": "forge-release-readiness-envelope/v1",
        "report": report,
        "receipt": {
            "format": RECEIPT_FORMAT,
            "report_sha256": hashlib.sha256(report_bytes).hexdigest(),
            "report_byte_length": len(report_bytes),
            "report_bytes_utf8": report_bytes.decode("utf-8"),
        },
    }


def _execute(value: object) -> dict:
    request = _validate_input(value)
    if request["mode"] == "fixture":
        snapshot = _fixture_snapshot(request["fixture_id"])
    else:
        snapshot = GitHubReader(request["repository"]).snapshot(request["revision"], request["tag"])
    return _envelope(_build_report(request, snapshot))


def _response(request: object) -> dict:
    if not isinstance(request, dict) or request.get("api_version") != 1:
        return {"api_version": 1, "error": {"code": "invalid_protocol_request"}}
    operation = request.get("operation")
    if operation == "describe":
        return {
            "api_version": 1,
            "description": "Deterministic readiness evidence for one pinned GitHub revision and release tag. Fixture mode is offline; live mode reads public GitHub API data only.",
            "schema": {
                "type": "object",
                "required": ["mode", "repository", "revision", "tag", "required_checks", "required_artifacts"],
                "properties": {
                    "mode": {"type": "string", "enum": ["fixture", "live"]},
                    "fixture_id": {"type": "string", "enum": sorted(FIXTURE_IDS)},
                    "repository": {"type": "string", "pattern": "^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$"},
                    "revision": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                    "tag": {"type": "string"},
                    "required_checks": {"type": "array", "items": {"type": "string"}, "minItems": 1, "uniqueItems": True},
                    "required_artifacts": {"type": "array", "minItems": 1, "items": {"type": "string"}, "uniqueItems": True},
                    "observed_at": {"type": "string", "format": "date-time"},
                },
                "additionalProperties": False,
            },
        }
    if operation == "execute":
        try:
            return {"api_version": 1, "output": _execute(request.get("input"))}
        except InputError as exc:
            return {"api_version": 1, "error": {"code": exc.code}}
        except InvalidEvidence:
            report_input = request.get("input")
            if not isinstance(report_input, dict):
                return {"api_version": 1, "error": {"code": "invalid_evidence"}}
            try:
                normalized = _validate_input(report_input)
                snapshot = {name: {"state": "unverified"} for name in ("checks", "tag", "release")}
                report = _build_report(normalized, snapshot)
                report["collection_status"] = "partial"
                report["verdict"] = "incomplete"
                return {"api_version": 1, "output": _envelope(report)}
            except InputError as exc:
                return {"api_version": 1, "error": {"code": exc.code}}
        except ApiUnavailable as exc:
            return {"api_version": 1, "error": {"code": exc.code}}
    if operation == "close":
        return {"api_version": 1, "output": {}}
    return {"api_version": 1, "error": {"code": "unsupported_operation"}}


def main() -> int:
    raw = sys.stdin.buffer.read(MAX_INPUT_BYTES + 1)
    if len(raw) > MAX_INPUT_BYTES:
        response = {"api_version": 1, "error": {"code": "request_too_large"}}
    else:
        try:
            request = json.loads(raw.decode("utf-8"))
            response = _response(request)
        except (UnicodeDecodeError, json.JSONDecodeError):
            response = {"api_version": 1, "error": {"code": "invalid_json_request"}}
    sys.stdout.write(json.dumps(response, sort_keys=True, separators=(",", ":")) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
