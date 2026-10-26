#!/usr/bin/env python3
"""Capture PR #3's exact base/head on a disposable macOS Actions runner."""

import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile


BASE = "962a3207c31f0f955ac74d37510133f82289e0e4"
HEAD = "f44b90c8e8bba86d6a5808717636ebe527d92364"
SCENES = ("cold-open", "revisit", "outage", "outage-draft", "side-chat", "fork")
IMAGES = (*SCENES[:-1], "fork-waiting", "fork-failed")


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


def capture_hooks(base, head):
    """Only port the fixture entry points; retain the base's production behavior."""
    for file, start, anchor in (
        ("shell.rs", "    /// A session row's click in the sidebar.",
         "    pub fn fixture_transcript(&self) -> Entity<crate::transcript::Transcript> {"),
        ("composer.rs", "    /// A typed draft, as the input holds it.",
         "    pub fn fixture_clear_appshots(&mut self, cx: &mut Context<Self>) {"),
    ):
        source = (head / "crates/ui/src" / file).read_text()
        begin = source.index(start)
        hooks = source[begin:source.index("\n}\n", begin)] + "\n"
        path = base / "crates/ui/src" / file
        content = path.read_text()
        assert content.count(anchor) == 1, f"ambiguous capture hook in {path}"
        assert start not in content, f"capture hooks already present in {path}"
        path.write_text(content.replace(anchor, hooks + anchor))
    fixture = Path("crates/ui/examples/remote-sync-fixture.rs")
    shutil.copyfile(head / fixture, base / fixture)
    manifest = base / "crates/ui/Cargo.toml"
    assert 'name = "remote-sync-fixture"' not in manifest.read_text()
    with manifest.open("a") as stream:
        stream.write('\n[[example]]\nname = "remote-sync-fixture"\n'
                     'required-features = ["appshots-fixture"]\n')


def main():
    repo = Path(os.environ["GITHUB_WORKSPACE"]).resolve()
    output = Path(os.environ["CAPTURE_OUTPUT"]).resolve()
    output.mkdir(parents=True, exist_ok=False)
    owned = Path(tempfile.mkdtemp(prefix="remote-sync-sources-", dir=os.environ["RUNNER_TEMP"]))
    subprocess.run(["git", "-C", str(repo), "fetch", "--no-tags", "--depth=1",
                    "origin", BASE, HEAD], check=True)
    trees = {"base": owned / "base", "head": owned / "head"}
    for label, sha in (("base", BASE), ("head", HEAD)):
        subprocess.run(["git", "-C", str(repo), "worktree", "add", "--detach",
                        str(trees[label]), sha], check=True)
    capture_hooks(trees["base"], trees["head"])
    expected = {"crates/ui/Cargo.toml", "crates/ui/src/shell.rs", "crates/ui/src/composer.rs"}
    assert set(git(trees["base"], "diff", "--name-only").splitlines()) == expected
    assert not git(trees["head"], "status", "--porcelain"), "head must stay unmodified"
    (output / "base-fixture-only.patch").write_text(git(trees["base"], "diff") + "\n")
    (output / "remote-sync-fixture.rs").write_text(
        (trees["head"] / "crates/ui/examples/remote-sync-fixture.rs").read_text())
    provenance = {
        "base": BASE, "head": HEAD,
        "base_modifications": sorted(expected | {"crates/ui/examples/remote-sync-fixture.rs"}),
        "scenario": "Production Shell and real isolated engine; scripted host/connectivity. No real remote host.",
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "captures": {}, "executables": {}, "complete": False,
    }
    record = output / "provenance.json"
    record.write_text(json.dumps(provenance, indent=2) + "\n")
    try:
        for label, tree in trees.items():
            # Cargo can reuse a path dependency built from another checkout when
            # both worktrees share a target directory. Keep each build isolated.
            target = Path(os.environ["CARGO_TARGET_DIR"]) / label
            build_env = dict(os.environ, CARGO_TARGET_DIR=str(target))
            with (output / f"{label}-build.log").open("w") as log:
                subprocess.run(["cargo", "build", "--locked", "-p", "zeron-ui", "--features",
                                "appshots-fixture", "--example", "remote-sync-fixture"],
                               cwd=tree, env=build_env, stdout=log,
                               stderr=subprocess.STDOUT, check=True)
            destination = output / label
            executable = destination / "Fixture.app/Contents/MacOS/fixture"
            executable.parent.mkdir(parents=True)
            shutil.copy2(target / "debug/examples/remote-sync-fixture", executable)
            provenance["executables"][label] = hashlib.sha256(executable.read_bytes()).hexdigest()
            if label == "head":
                assert provenance["executables"]["base"] != provenance["executables"]["head"], \
                    "base and head executables unexpectedly match"
            info = plistlib.loads((tree / "dist/macos/Info.plist").read_bytes().replace(
                b"__VERSION__", b"1.0.0"))
            info["CFBundleExecutable"] = "fixture"
            info["CFBundleIdentifier"] = f"sh.zeron.remote-sync-fixture.{label}"
            (destination / "Fixture.app/Contents/Info.plist").write_bytes(plistlib.dumps(info))
            for scene in SCENES:
                with (output / f"{label}-{scene}.log").open("w") as log:
                    subprocess.run([str(executable), str(destination), scene], cwd=tree,
                                   stdout=log, stderr=subprocess.STDOUT, timeout=90, check=True)
            captures = {}
            for name in IMAGES:
                path = destination / f"{name}.png"
                image = path.read_bytes()
                assert image[:8] == b"\x89PNG\r\n\x1a\n", f"not a PNG: {path}"
                width = int.from_bytes(image[16:20], "big")
                height = int.from_bytes(image[20:24], "big")
                assert width > 0 and height > 0
                captures[name] = {"sha256": hashlib.sha256(image).hexdigest(),
                                  "width": width, "height": height}
            provenance["captures"][label] = captures
            record.write_text(json.dumps(provenance, indent=2) + "\n")
        provenance["complete"] = True
    except Exception as error:
        provenance["error"] = str(error)
        raise
    finally:
        record.write_text(json.dumps(provenance, indent=2) + "\n")


if __name__ == "__main__":
    main()
