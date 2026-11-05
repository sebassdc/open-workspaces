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
const stateFile = process.env.OW_RELEASE_STATE_FILE;
const state = fs.readFileSync(stateFile, "utf8");
if (args[1] === "view") {
  if (state === "missing") process.exit(1);
  process.stdout.write(String(state === "draft"));
}
if (args[1] === "create") {
  const failures = Number(fs.readFileSync(stateFile + ".fail", "utf8"));
  if (args.includes("--generate-notes") && failures > 0) {
    fs.writeFileSync(stateFile + ".fail", String(failures - 1));
    process.exit(1);
  }
  fs.writeFileSync(stateFile, "draft");
}
`);
    chmodSync(gh, 0o700);
    const tag = `v${JSON.parse(read("../package.json")).version}`;
    try {
        const stateFile = join(directory, "state");
        const view = ["release", "view", tag, "--json", "isDraft", "--jq", ".isDraft"];
        const generated = ["release", "create", tag, "--verify-tag", "--generate-notes", "--draft"];
        // [state, notes service failures, calls]: a draft or published release is left as it is;
        // a failing notes service is retried (no sleeps here), then the commit message is the notes.
        const cases = [
            ["missing", 0, [view, generated, view]],
            ["draft", 0, [view, view]],
            ["published", 0, [view, view]],
            ["missing", 2, [view, generated, view, view, generated, view, view, generated, view]],
        ];
        for (const [state, failures, expected] of cases) {
            writeFileSync(log, "");
            writeFileSync(stateFile, state);
            writeFileSync(stateFile + ".fail", String(failures));
            execFileSync("bash", ["-e", "-c", script], {
                cwd: new URL("../", import.meta.url),
                env: { ...process.env, PATH: `${directory}:${process.env.PATH}`, GH_TOKEN: "test-only", GH_REPO: "fixture/repo", OW_RELEASE_STATE_FILE: stateFile, OW_RELEASE_LOG: log, OW_RELEASE_WAIT: "0" },
            });
            const calls = readFileSync(log, "utf8").trim().split("\n").map((line) => JSON.parse(line));
            assert.deepEqual(calls, expected, `${state}, ${failures} notes failures`);
        }
        // The notes service never answers: the release is made with the commit message.
        writeFileSync(log, "");
        writeFileSync(stateFile, "missing");
        writeFileSync(stateFile + ".fail", "99");
        execFileSync("bash", ["-e", "-c", script], {
            cwd: new URL("../", import.meta.url),
            env: { ...process.env, PATH: `${directory}:${process.env.PATH}`, GH_TOKEN: "test-only", GH_REPO: "fixture/repo", OW_RELEASE_STATE_FILE: stateFile, OW_RELEASE_LOG: log, OW_RELEASE_WAIT: "0", GITHUB_SHA: "HEAD", RUNNER_TEMP: directory },
        });
        const fallback = readFileSync(log, "utf8").trim().split("\n").map((line) => JSON.parse(line));
        assert.deepEqual(fallback.filter((c) => c[1] === "create").at(-1), ["release", "create", tag, "--verify-tag", "--notes-file", join(directory, "notes.md"), "--draft"]);
        assert.equal(readFileSync(stateFile, "utf8"), "draft");
    } finally {
        rmSync(directory, { recursive: true, force: true });
    }
});


test("release assets gate tagging and publication", () => {
    const workflow = read("../../../.github/workflows/aws-component-release.yml");
    assert.match(workflow, /\n  tag:\n    if: github.event_name == 'push'\n    needs: \[clients, checks\]/);
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
