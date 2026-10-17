import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { test } from "node:test";

// release-impact.json is the contract ow-host upgrade plans from (docs/UPGRADES.md).
const read = (path) => readFileSync(new URL(`../../../${path}`, import.meta.url), "utf8");
const manifest = JSON.parse(read("release-impact.json"));
const release = JSON.parse(read("deploy/aws/package.json")).version;
const CLASS = { component: 0, config: 0, "guest-agent": 0, "host-layout": 0, dashboard: 1, controller: 1, agent: 1,
    worker: 2, vmm: 2, "guest-kernel": 2, stage0: 3 };
const semver = (v) => v.split(".").map(Number);
const newer = (a, b) => { // a > b
    const [x, y] = [semver(a), semver(b)];
    for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] > y[i];
    return false;
};

test("the manifest is ordered, complete for this release and self-consistent", () => {
    assert.equal(manifest.schema, 1);
    const versions = manifest.releases.map((e) => e.version);
    for (const v of versions) assert.match(v, /^\d+\.\d+\.\d+$/);
    for (let i = 1; i < versions.length; i++) assert.ok(newer(versions[i - 1], versions[i]), `${versions[i - 1]} before ${versions[i]}, newest first`);
    assert.equal(versions[0], release, "the newest entry is deploy/aws/package.json's version");
    for (const e of manifest.releases) {
        for (const change of e.changes) assert.ok(change in CLASS, `${e.version}: unknown change ${change}`);
        const highest = Math.max(0, ...e.changes.map((c) => CLASS[c]));
        if (e.changes.includes("host-layout") && !e.changes.includes("stage0")) {
            assert.ok(e.class >= highest && e.class <= 2, `${e.version}: class ${e.class} with host-layout`);
        } else {
            assert.equal(e.class, highest, `${e.version}: class is the highest of its changes`);
        }
        assert.equal(e.class === 3, e.in_place_from === null, `${e.version}: in_place_from is null exactly for class 3`);
        if (e.in_place_from) {
            assert.ok(versions.includes(e.in_place_from) && !newer(e.in_place_from, e.version), `${e.version}: in_place_from`);
        }
        const [lo, hi] = e.protocol.accepts;
        assert.ok(lo <= e.protocol.speaks && e.protocol.speaks <= hi, `${e.version}: a controller accepts its own protocol`);
        assert.ok(e.catalog.min_reader <= e.catalog.schema, `${e.version}: catalog floor`);
        assert.ok(e.runtime.resumes.includes(e.runtime.vmm), `${e.version}: a worker resumes its own captures`);
        assert.match(e.runtime.guest_kernel_sha256, /^[0-9a-f]{64}$/);
    }
});

test("this release's entry matches the code", () => {
    const entry = manifest.releases.find((e) => e.version === release);
    const common = read("crates/ow-core/src/common.rs");
    const constant = (name) => common.match(new RegExp(`^pub const ${name}: [^=]+= ([^;]+);`, "m"))?.[1];
    assert.equal(entry.protocol.speaks, Number(constant("PROTOCOL")));
    assert.deepEqual(entry.protocol.accepts, [Number(constant("PROTOCOL_MIN")), Number(constant("PROTOCOL"))]);
    assert.equal(entry.worker_protocol.speaks, Number(constant("WORKER_PROTOCOL")));
    assert.deepEqual(entry.worker_protocol.accepts, [Number(constant("WORKER_PROTOCOL_MIN")), Number(constant("WORKER_PROTOCOL"))]);
    assert.equal(JSON.stringify(entry.runtime.vmm), constant("RUNTIME"), "the worker's runtime");
    assert.ok(constant("RUNTIMES").includes(JSON.stringify(entry.runtime.vmm)), "the controller admits it");
    assert.deepEqual(entry.runtime.resumes, [entry.runtime.vmm], "Snapshot::resumable compares common::RUNTIME only");
    assert.match(read("crates/ow-core/src/runtime.rs"), /self\.runtime == crate::common::RUNTIME/);
    assert.equal(entry.catalog.schema, Number(read("crates/ow/src/nodes.rs").match(/^pub\(crate\) const SCHEMA: i64 = (\d+);/m)[1]));
    const kernel = read("experiments/runtime-spike/prepare-guest.py").match(/'vmlinux-[^']+': \(\s*'[^']+',\s*'([0-9a-f]{64})'/)[1];
    assert.equal(entry.runtime.guest_kernel_sha256, kernel, "the guest kernel pin in prepare-guest.py");
});

// What a diff since the previous release requires the entries in between to declare.
const RULES = [
    [/^deploy\/aws\/bootstrap\.sh$/, ["stage0"]],
    [/^deploy\/aws\/host\//, ["host-layout"]],
    [/^crates\/ow\/src\/host\.rs$|^crates\/ow-core\/src\/(runtime|vm|network|ssh_guest|idle)\.rs$/, ["worker"]],
    [/^crates\/ow\/src\/(remote|dashboard|terminal|ssh_gateway)\.rs$|^crates\/ow\/ui\//, ["dashboard"]],
    [/^crates\/ow\/src\/(nodes|catalog|selfhost)\.rs$/, ["controller", "agent"]],
    [/^experiments\/runtime-spike\/fetch-firecracker\.sh$/, ["vmm"]],
];

test("the entries since the previous release tag declare what the diff changed", (t) => {
    let tags;
    try {
        tags = execFileSync("git", ["tag", "-l", "v*"], { encoding: "utf8" }).split("\n").filter((x) => /^v\d+\.\d+\.\d+$/.test(x));
    } catch {
        return t.skip("no git");
    }
    const previous = tags.map((x) => x.slice(1)).filter((v) => newer(release, v)).sort((a, b) => (newer(a, b) ? -1 : 1))[0];
    if (!previous) return t.skip("no earlier release tag (shallow clone?)");
    const files = execFileSync("git", ["diff", "--name-only", `v${previous}`, "HEAD"], { encoding: "utf8" }).split("\n").filter(Boolean);
    const entries = manifest.releases.filter((e) => newer(e.version, previous));
    const declared = new Set(entries.flatMap((e) => [...e.changes, ...Object.keys(e.waivers ?? {})]));
    for (const [pattern, anyOf] of RULES) {
        const hit = files.filter((f) => pattern.test(f));
        if (hit.length) {
            assert.ok(anyOf.some((c) => declared.has(c)), `${hit.join(", ")} changed since v${previous}: declare ${anyOf.join(" or ")} (or a waiver with the reason)`);
        }
    }
    const pin = (rev) => execFileSync("git", ["show", `${rev}:experiments/runtime-spike/prepare-guest.py`], { encoding: "utf8" })
        .match(/'vmlinux-[^']+': \(\s*'[^']+',\s*'([0-9a-f]{64})'/)?.[1];
    if (pin(`v${previous}`) !== pin("HEAD")) assert.ok(declared.has("guest-kernel"), "the guest kernel pin changed: declare guest-kernel");
});
