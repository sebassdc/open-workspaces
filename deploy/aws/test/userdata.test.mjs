import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { renderUserData } from "../index.ts";

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
