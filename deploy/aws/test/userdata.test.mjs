import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { dashboardLoginList, developerImageList, renderUserData } from "../index.ts";

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

test("the controller waits for the tailnet before choosing its listen address", () => {
    assert.ok(bootstrap.includes("ExecStartPre=/usr/local/lib/open-workspaces/wait-tailnet"));
    const wait = bootstrap.slice(bootstrap.indexOf("<<'WAIT'\n") + 8, bootstrap.indexOf("\nWAIT\n"));
    const dir = mkdtempSync(join(tmpdir(), "ow-wait-"));
    // Stand-in tailscale: reports Running only after its second status call.
    writeFileSync(join(dir, "tailscale"), `#!/bin/sh
case "$1" in
  status) n=$(cat "${dir}/n" 2>/dev/null || echo 0); echo $((n + 1)) > "${dir}/n"
          if [ "$STATE" = Starting ] && [ "$n" -ge 1 ]; then s=Running; else s=$STATE; fi
          echo "{\\"BackendState\\": \\"$s\\"}" ;;
  ip) [ "$STATE" = Starting ] ;;
esac
`, { mode: 0o755 });
    const run = (state) => {
        rmSync(join(dir, "n"), { force: true });
        const started = Date.now();
        execFileSync("sh", ["-c", wait], { env: { ...process.env, PATH: `${dir}:${process.env.PATH}`, STATE: state } });
        return { calls: Number(readFileSync(join(dir, "n"), "utf8")), ms: Date.now() - started };
    };
    assert.equal(run("Starting").calls, 2, "waits until the tailnet is Running");
    assert.ok(run("NeedsLogin").ms < 1000, "a host that is not joined does not wait");
    rmSync(dir, { recursive: true });
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

test("the tailnet identity is kept on the data volume, mounted before tailscaled starts", () => {
    const mount = bootstrap.indexOf("mountpoint -q /var/lib/tailscale || mount /var/lib/tailscale");
    assert.ok(bootstrap.indexOf("mountpoint -q /srv/ow || mount /srv/ow") < mount, "data volume first");
    assert.ok(mount > 0 && mount < bootstrap.indexOf("apt-get install -y -q tailscale"), "state mounted before tailscale is installed");
    assert.ok(bootstrap.includes("RequiresMountsFor=/var/lib/tailscale"), "tailscaled waits for the state mount at boot");
    assert.ok(bootstrap.includes("chmod 700 /srv/ow/.tailscale-state"));
    assert.ok(bootstrap.includes('--ssh="$([[ -n $OW_TAILSCALE_SSH ]] && echo true || echo false)"'), "restored state cannot keep an old SSH setting");
});

test("stopping the service hibernates running boxes before ow down", () => {
    assert.ok(bootstrap.includes("ExecStop=/usr/local/lib/open-workspaces/stop"));
    assert.ok(bootstrap.includes("TimeoutStopSec=900"));
    const stop = bootstrap.slice(bootstrap.indexOf("<<'STOP'\n") + 8, bootstrap.indexOf("\nSTOP\n"));
    const dir = mkdtempSync(join(tmpdir(), "ow-stop-"));
    const log = join(dir, "log");
    // Stand-in ow: two running boxes (one fails to hibernate) and a stopped one.
    const fake = `#!/bin/sh
echo "$*" >> "${log}"
case "$1" in
  list) echo '[{"id":"a","state":"running"},{"id":"b","state":"stopped"},{"id":"c","state":"running"}]' ;;
  hibernate) [ "$2" != c ] ;;
esac
`;
    writeFileSync(join(dir, "ow"), fake, { mode: 0o755 });
    const script = stop.replaceAll("/usr/local/bin/ow", join(dir, "ow")).replace("/srv/ow/src/target/release/ow", join(dir, "ow"));
    const out = execFileSync("sh", ["-c", script], { encoding: "utf8" });
    assert.equal(readFileSync(log, "utf8"), "list\nhibernate a\nhibernate c\n--data-dir /srv/ow/.ow/runner down\n");
    assert.match(out, /hibernated a/);
    assert.match(out, /could not hibernate c/, "a failed hibernate still reaches ow down");
    rmSync(dir, { recursive: true });
});

test("developer images are validated and built by a oneshot unit", () => {
    assert.equal(developerImageList(["ubuntu", "ubuntu"]), "ubuntu");
    assert.equal(developerImageList([]), "");
    for (const bad of ["alpine", "arch", "ubuntu; id", ""]) {
        assert.throws(() => developerImageList([bad]), /developerImages takes ubuntu/);
    }
    const script = renderUserData({ OW_DEVELOPER_IMAGES: "ubuntu", OW_TAILSCALE_SSH: "" }, bootstrap);
    execFileSync("bash", ["-n"], { input: script });
    const images = bootstrap.slice(bootstrap.indexOf("<<'IMAGES'\n") + 10, bootstrap.indexOf("\nIMAGES\n"));
    execFileSync("bash", ["-n"], { input: images });
    assert.ok(bootstrap.includes("ExecStart=/usr/local/lib/open-workspaces/developer-images $OW_DEVELOPER_IMAGES"));
});
