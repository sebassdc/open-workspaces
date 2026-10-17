import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
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
