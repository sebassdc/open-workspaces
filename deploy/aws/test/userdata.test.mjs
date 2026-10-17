import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { backupLocation, backupPolicy, dashboardLoginList, developerImageList, renderUserData } from "../index.ts";

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

test("stopping the service hibernates running boxes, with wakes refused", () => {
    assert.ok(bootstrap.includes("ExecStop=/srv/ow/src/target/release/ow --data-dir /srv/ow/.ow/runner down --hibernate"));
    assert.ok(bootstrap.includes("TimeoutStopSec=900"));
});

test("developer images are validated and built by a oneshot unit", () => {
    assert.equal(developerImageList(["ubuntu", "ubuntu"]), "ubuntu");
    assert.equal(developerImageList([]), "");
    for (const bad of ["alpine", "arch", "ubuntu; id", ""]) {
        assert.throws(() => developerImageList([bad]), /developerImages takes ubuntu/);
    }
    const script = renderUserData({ OW_DEVELOPER_IMAGES: "ubuntu", OW_TAILSCALE_SSH: "" }, bootstrap);
    execFileSync("bash", ["-n"], { input: script });
    const images = bootstrap.slice(bootstrap.indexOf("<<'IMAGES'\n") + 11, bootstrap.indexOf("\nIMAGES\n"));
    execFileSync("bash", ["-n"], { input: images });
    assert.ok(bootstrap.includes("ExecStart=/usr/local/lib/open-workspaces/developer-images $OW_DEVELOPER_IMAGES"));
});

test("backup URLs are checked and the host role can never delete a backup", () => {
    assert.deepEqual(backupLocation("s3://my-bucket/ow/host-1/"), { bucket: "my-bucket", prefix: "ow/host-1" });
    for (const bad of ["s3://my-bucket", "s3://my-bucket/", "https://my-bucket/ow", "s3://My_Bucket/ow", "s3://b/ow",
        "s3://my-bucket/ow/../x", "s3://my-bucket/a//b", "s3://my-bucket/ow'x", "s3://my-bucket/$(id)"]) {
        assert.throws(() => backupLocation(bad), /backupUrl must be/, bad);
    }
    const policy = JSON.parse(backupPolicy("aws", "s3://my-bucket/ow/host-1"));
    assert.deepEqual(policy.Statement.flatMap((s) => [s.Action].flat()).sort(),
        ["s3:AbortMultipartUpload", "s3:GetObject", "s3:ListBucket", "s3:PutObject"]);
    assert.equal(policy.Statement[0].Resource, "arn:aws:s3:::my-bucket");
    assert.deepEqual(policy.Statement[0].Condition, { StringLike: { "s3:prefix": ["ow/host-1/*"] } });
    assert.equal(policy.Statement[1].Resource, "arn:aws:s3:::my-bucket/ow/host-1/*");
});

test("ow-backup uploads the catalog, then the snapshots, and stops at the first failure", () => {
    const script = renderUserData({ OW_BACKUP_URL: "s3://my-bucket/ow/host-1", OW_TAILSCALE_SSH: "" }, bootstrap);
    execFileSync("bash", ["-n"], { input: script });
    const dir = mkdtempSync(join(tmpdir(), "ow-backup-"));
    writeFileSync(join(dir, "backup.env"), "OW_BACKUP_URL=s3://my-bucket/ow/host-1\n");
    const backup = bootstrap.slice(bootstrap.indexOf("<<'BACKUP'\n") + 11, bootstrap.indexOf("\nBACKUP\n"))
        .replace("export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", `export PATH=${dir}:/usr/bin:/bin`)
        .replace("/etc/open-workspaces/backup.env", join(dir, "backup.env"))
        .replaceAll("/srv/ow/.ow/backups", join(dir, "backups"));
    // Stand-ins log their arguments; the file "fail" names the one that exits 1.
    for (const tool of ["ow", "aws"]) {
        writeFileSync(join(dir, tool), `#!/bin/sh
[ "${tool}" = aws ] && env | grep '^AWS_' | sort > "${dir}/aws-env"
echo "${tool} $*" >> "${dir}/log"
[ "$(cat "${dir}/fail")" = "${tool} $1" ] && exit 1
[ "$1" = backup ] && [ "$2" = catalog ] && touch "$4"
exit 0
`, { mode: 0o755 });
    }
    writeFileSync(join(dir, "install"), `#!/bin/sh\nfor a; do d=$a; done; mkdir -p "$d"\n`, { mode: 0o755 });
    writeFileSync(join(dir, "runuser"), `#!/bin/sh\nshift 3; exec "$@"\n`, { mode: 0o755 });
    const run = (fail) => {
        rmSync(join(dir, "log"), { force: true });
        writeFileSync(join(dir, "fail"), fail);
        let code = 0;
        try {
            execFileSync("bash", ["-c", backup], {
                env: { ...process.env, AWS_ENDPOINT_URL: "http://127.0.0.1:9", AWS_PROFILE: "evil" },
                stdio: "pipe",
            });
        } catch (e) {
            code = e.status;
        }
        const log = readFileSync(join(dir, "log"), "utf8").trim().split("\n");
        return { code, log };
    };
    const ok = run("");
    assert.equal(ok.code, 0);
    assert.match(ok.log[0], /^ow backup catalog --output .*\/backups\/catalog-\d{8}T\d{6}Z\.sqlite3$/);
    assert.match(ok.log[1], /^aws s3 cp --only-show-errors .*catalog-\d{8}T\d{6}Z\.sqlite3 s3:\/\/my-bucket\/ow\/host-1\/catalog\/catalog-\d{8}T\d{6}Z\.sqlite3$/);
    assert.equal(ok.log[2], "ow backup snapshots --to s3://my-bucket/ow/host-1/snapshots");
    assert.equal(readFileSync(join(dir, "aws-env"), "utf8"), "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS=true\n",
        "the catalog upload ignores endpoints from the environment and profiles");
    assert.deepEqual(readdirSync(join(dir, "backups")), [], "the local catalog copy is removed");
    const upload = run("aws s3");
    assert.notEqual(upload.code, 0);
    assert.equal(upload.log.length, 2, "a failed catalog upload stops before the snapshots");
    assert.notEqual(run("ow backup").code, 0, "a failed catalog copy fails the run");
    rmSync(dir, { recursive: true });
});
