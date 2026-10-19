import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

// Consumers install this package with `pulumi package add`, which generates their SDK from
// this schema; 0.6.0 shipped a Runner the analyzer could not find. Needs the Pulumi CLI.
test("pulumi package get-schema finds every component", (t) => {
    try {
        execFileSync("pulumi", ["version"], { stdio: "ignore" });
    } catch {
        t.skip("no pulumi CLI");
        return;
    }
    const dir = fileURLToPath(new URL("..", import.meta.url));
    const schema = JSON.parse(execFileSync("pulumi", ["package", "get-schema", dir], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }));
    for (const name of ["Host", "Runner", "ArtifactStore", "RunnerPool"]) {
        assert.ok(schema.resources?.[`open-workspaces-aws:index:${name}`]?.isComponent, `${name} in the schema`);
    }
    for (const name of ["Host", "Runner"]) {
        const inputs = schema.resources[`open-workspaces-aws:index:${name}`].inputProperties;
        assert.equal(inputs.vcpuOvercommit.type, "number");
        assert.equal(inputs.storeUrl.type, "string");
        assert.equal(inputs.imageArtifacts.additionalProperties.$ref, "#/types/open-workspaces-aws:index:ImageArtifact");
        assert.equal(schema.resources[`open-workspaces-aws:index:${name}`].inputProperties.tailscaleDirect.$ref,
            "#/types/open-workspaces-aws:index:TailscaleDirectArgs");
    }
    assert.equal(schema.resources["open-workspaces-aws:index:Host"].inputProperties.role.type, "string");
    assert.ok(!("role" in schema.resources["open-workspaces-aws:index:Runner"].inputProperties));
    assert.equal(schema.resources["open-workspaces-aws:index:Runner"].inputProperties.controller.$ref, "#/types/open-workspaces-aws:index:RunnerController");
    assert.equal(schema.resources["open-workspaces-aws:index:Host"].inputProperties.dashboardCapability.type, "string");
    assert.equal(schema.resources["open-workspaces-aws:index:Host"].inputProperties.storeGc.type, "boolean");
    assert.ok(!("storeGc" in schema.resources["open-workspaces-aws:index:Runner"].inputProperties));
    assert.ok(!("dashboardCapability" in schema.resources["open-workspaces-aws:index:Runner"].inputProperties));
    const pool = schema.resources["open-workspaces-aws:index:RunnerPool"];
    // A pool never follows a moving image, and the controller alone sizes it.
    for (const input of ["amiId", "controller", "maxSize", "instanceHourlyCents", "diskMonthlyCentsPerGib", "tailscaleAuthKeySecretArn"]) {
        assert.ok(pool.requiredInputs.includes(input), `RunnerPool requires ${input}`);
    }
    for (const input of ["desiredCapacity", "minSize", "role", "dashboardLogins"]) assert.ok(!(input in pool.inputProperties), input);
});
