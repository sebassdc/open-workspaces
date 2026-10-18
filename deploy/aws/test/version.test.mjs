import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, mkdtempSync, writeFileSync, chmodSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

const read = (path) => readFileSync(new URL(path, import.meta.url), "utf8");

// Releases are tagged from package.json; `ow --version` must report the same release.
test("crate versions and Cargo.lock match the release version", () => {
    const release = JSON.parse(read("../package.json")).version;
    const workspace = read("../../../Cargo.toml").match(/\[workspace\.package\][^[]*?^version = "([^"]+)"/m);
    assert.equal(workspace?.[1], release, "Cargo.toml [workspace.package] version");
    for (const crate of ["ow", "ow-core", "ow-guest"]) {
        assert.match(read(`../../../crates/${crate}/Cargo.toml`), /^version\.workspace = true$/m, `${crate} inherits the workspace version`);
        const locked = read("../../../Cargo.lock").match(new RegExp(`^name = "${crate}"\\nversion = "([^"]+)"`, "m"));
        assert.equal(locked?.[1], release, `Cargo.lock entry for ${crate}`);
    }
});

test("an existing release tag must name this commit", () => {
    const tag = `v${JSON.parse(read("../package.json")).version}`;
    if (!execFileSync("git", ["tag", "--list", tag], { encoding: "utf8" }).trim()) return;
    const commit = (ref) => execFileSync("git", ["rev-parse", ref], { encoding: "utf8" }).trim();
    assert.equal(commit(`${tag}^{commit}`), commit("HEAD"), `${tag} already names different code; choose a fresh release version`);
});


test("main stages a release only after tagging; retries preserve published releases", () => {
    const workflow = read("../../../.github/workflows/aws-component-release.yml");
    const step = workflow.split("      - name: Prepare GitHub release\n")[1];
    assert.ok(step, "a tag alone is not a published GitHub Release");
    assert.match(step, /if: github.event_name == 'push'/);
    assert.match(workflow, /push:\n    branches: \[main\]/);
    assert.ok(workflow.indexOf("- name: Tag the release") < workflow.indexOf("- name: Prepare GitHub release"));
    const script = step.split("        run: |\n")[1].split("\n      - ")[0].split("\n").map((line) => line.slice(10)).join("\n");
    const directory = mkdtempSync(join(tmpdir(), "ow-release-"));
    const log = join(directory, "calls.jsonl");
    const gh = join(directory, "gh");
    writeFileSync(gh, `#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
fs.appendFileSync(process.env.OW_RELEASE_LOG, JSON.stringify(args) + String.fromCharCode(10));
if (args[1] === "view") {
  if (process.env.OW_RELEASE_STATE === "missing") process.exit(1);
  process.stdout.write(String(process.env.OW_RELEASE_STATE === "draft"));
}
`);
    chmodSync(gh, 0o700);
    const tag = `v${JSON.parse(read("../package.json")).version}`;
    try {
        for (const state of ["missing", "draft", "published"]) {
            writeFileSync(log, "");
            execFileSync("bash", ["-e", "-c", script], {
                cwd: new URL("../", import.meta.url),
                env: { ...process.env, PATH: `${directory}:${process.env.PATH}`, GH_TOKEN: "test-only", GH_REPO: "fixture/repo", OW_RELEASE_STATE: state, OW_RELEASE_LOG: log },
            });
            const calls = readFileSync(log, "utf8").trim().split("\n").map((line) => JSON.parse(line));
            const expected = [["release", "view", tag, "--json", "isDraft", "--jq", ".isDraft"]];
            if (state === "missing") expected.push(["release", "create", tag, "--verify-tag", "--generate-notes", "--draft"]);
            assert.deepEqual(calls, expected, state);
        }
    } finally {
        rmSync(directory, { recursive: true, force: true });
    }
});


test("release assets gate tagging and publication", () => {
    const workflow = read("../../../.github/workflows/aws-component-release.yml");
    assert.match(workflow, /check-and-tag:\n    needs: clients/);
    for (const artifact of ["ow-linux-amd64", "ow-darwin-arm64", "ow-darwin-amd64"]) {
        assert.ok(workflow.includes(`artifact: ${artifact}`));
    }
    const assembly = workflow.indexOf("- name: Assemble verified release assets");
    const tag = workflow.indexOf("- name: Tag the release");
    const attach = workflow.indexOf("- name: Attach clients");
    const publish = workflow.indexOf("- name: Publish complete GitHub release");
    assert.ok(assembly > 0 && assembly < tag && tag < attach && attach < publish);
    assert.match(workflow, /python3 scripts\/test-cli-install.py/);
    assert.match(workflow, /printf '%s\\n' "\$GITHUB_SHA" > source-commit.txt/);
});
