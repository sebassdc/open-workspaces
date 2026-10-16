import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { dashboardLoginList, renderUserData } from "../index.ts";

const bootstrap = readFileSync(new URL("../bootstrap.sh", import.meta.url), "utf8");

test("user data keeps values literal and starts with the bootstrap shebang", () => {
    for (const value of [`it's $(touch /tmp/pwned) "quoted"`, "release-$&", "release-$$", "a-$'-$`-$1"]) {
        const script = renderUserData({ OW_REF: value, OW_TAILSCALE_SSH: "" }, bootstrap);
        assert.ok(script.startsWith("#!/bin/bash\n"));
        assert.ok(!script.includes("# @OW_ENV@"));
        const header = script.split("\n").filter((line) => line.startsWith("OW_"));
        const printed = execFileSync("bash", ["-c", `${header.join("\n")}\nprintf %s "$OW_REF"`], { encoding: "utf8" });
        assert.equal(printed, value);
    }
});

test("the Tailscale key is piped and an empty key never reaches tailscale up", () => {
    const join = bootstrap.slice(bootstrap.indexOf("join_tailscale() {"), bootstrap.indexOf("tailscale up --timeout"));
    assert.ok(join.includes('--output text | cat > "$key"'), "snap aws writes nothing to a redirected file");
    assert.ok(join.includes('[[ ! -s "$key" ]]') && join.includes("return 1"), "empty key stops before tailscale up");
});

test("dashboard logins are validated and the dashboard block is valid bash", () => {
    assert.equal(dashboardLoginList(["a@example.com", "b.c+d@example.org"]), "a@example.com,b.c+d@example.org");
    assert.equal(dashboardLoginList([]), "");
    for (const bad of ["no-at-sign", "a@b,c@d", "x@y'", "a b@c", "$(id)@x"]) {
        assert.throws(() => dashboardLoginList([bad]), /invalid dashboard login/);
    }
    const script = renderUserData({ OW_DASHBOARD_LOGINS: "a@example.com", OW_TAILSCALE_SSH: "" }, bootstrap);
    execFileSync("bash", ["-n"], { input: script });
    assert.ok(script.includes("--listen 127.0.0.1:8787"), "dashboard stays on loopback");
    assert.ok(script.includes("tailscale serve --bg http://127.0.0.1:8787"), "served to the tailnet only");
    assert.ok(!script.includes("tailscale funnel"), "never public");
});
