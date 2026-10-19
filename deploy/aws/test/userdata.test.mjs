import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import * as pulumi from "@pulumi/pulumi";

// Pulumi runs index.ts as CommonJS; loaded here as an ES module it needs these two.
globalThis.__dirname = fileURLToPath(new URL("..", import.meta.url));
globalThis.require = createRequire(new URL("../index.ts", import.meta.url));

// Mocks before the component loads: resources record their inputs, data sources answer.
const created = [];
pulumi.runtime.setMocks({
    newResource: (args) => {
        created.push(args);
        return { id: `${args.name}-id`, state: { ...args.inputs, arn: `arn:aws:mock:::${args.name}` } };
    },
    call: (args) => ({
        "aws:ec2/getVpc:getVpc": { id: "vpc-1" },
        "aws:ec2/getInstanceTypeOfferings:getInstanceTypeOfferings": { locations: ["us-east-1a"] },
        "aws:ec2/getSubnets:getSubnets": { ids: ["subnet-1"] },
        "aws:ec2/getSubnet:getSubnet": { id: "subnet-1", vpcId: "vpc-1", availabilityZone: "us-east-1a" },
        "aws:index/getPartition:getPartition": { partition: "aws" },
        "aws:index/getRegion:getRegion": { region: "us-east-1", name: "us-east-1" },
        "aws:ssm/getParameter:getParameter": { value: "ami-1" },
    })[args.token] ?? {},
}, "project", "stack", false);
const { Host, backupLocation, backupPolicy, dashboardLoginList, developerImageList, hostConfig, renderUserData } =
    await import("../index.ts");

const bootstrap = readFileSync(new URL("../bootstrap.sh", import.meta.url), "utf8");
const file = (name) => readFileSync(new URL(`../host/files/${name}`, import.meta.url), "utf8");
const settle = () => new Promise((resolve) => setTimeout(resolve, 200));

/** The instance user data and the config parameter value of a Host built from args. */
async function deploy(args) {
    created.length = 0;
    new Host("ow-test", args);
    await settle();
    const instance = created.find((r) => r.type === "aws:ec2/instance:Instance");
    const parameter = created.find((r) => r.type === "aws:ssm/parameter:Parameter");
    return { userData: instance.inputs.userData, config: JSON.parse(parameter.inputs.insecureValue), parameter, instance };
}

const secretArn = "arn:aws:secretsmanager:us-east-1:111122223333:secret:ow/ts-AbCdEf";

test("config and release changes leave user data byte-identical: they never replace the host", async () => {
    const a = await deploy({ ref: "v0.5.0" });
    const b = await deploy({
        ref: "v0.5.1", repository: "https://example.com/fork.git", tailscaleAuthKeySecretArn: secretArn,
        tailscaleHostname: "other", tailscaleSsh: true, dashboardLogins: ["you@example.com"], developerImages: ["ubuntu"],
        backupUrl: "s3://my-bucket/ow/x", backupSchedule: "*-*-* 01:00:00 UTC", idleMinutes: 5, vcpuOvercommit: 2,
    });
    assert.equal(a.userData, b.userData);
    assert.equal(a.config.vcpuOvercommit, 1);
    assert.equal(b.config.vcpuOvercommit, 2);
    assert.equal(a.instance.inputs.userDataReplaceOnChange, true, "a stage 0 change still replaces honestly");
    for (const key of ["OW_DATA_VOLUME_ID", "OW_CONFIG_PARAMETER", "OW_REGION", "OW_STAGE0"]) {
        assert.match(a.userData, new RegExp(`^${key}='`, "m"));
    }
    assert.ok(!a.userData.includes("v0.5.0"), "the release ref is not in user data");
    assert.equal(a.parameter.inputs.name, "/open-workspaces/ow-test/config");
    assert.equal(a.parameter.inputs.type, "String", "not secret: it holds only the Tailscale secret's ARN");
    assert.deepEqual(b.config, {
        schema: 1, role: "host",
        release: { repository: "https://example.com/fork.git", ref: "v0.5.1" },
        tailscale: { secretArn, hostname: "other", ssh: true },
        dashboard: { logins: ["you@example.com"] },
        developerImages: ["ubuntu"],
        backup: { url: "s3://my-bucket/ow/x", schedule: "*-*-* 01:00:00 UTC" },
        idleMinutes: 5, vcpuOvercommit: 2,
    });
    assert.equal(a.config.release.ref, "v0.5.0");
    assert.equal(a.config.tailscale.hostname, "ow-test");
    const read = created.find((r) => r.type === "aws:iam/rolePolicy:RolePolicy" && r.name === "ow-test-config");
    assert.deepEqual(JSON.parse(read.inputs.policy).Statement,
        [{ Effect: "Allow", Action: "ssm:GetParameter", Resource: "arn:aws:mock:::ow-test-config" }]);
});

test("the config is validated in the component", () => {
    const base = { repository: "https://github.com/x/y.git", ref: "v1", tailscaleAuthKeySecretArn: "", tailscaleHostname: "h",
        tailscaleSsh: false, dashboardLogins: [], developerImages: [], backupUrl: "", backupSchedule: "", idleMinutes: 30 };
    assert.doesNotThrow(() => hostConfig(base));
    for (const [bad, message] of [
        [{ dashboardLogins: ["a@example.com"] }, /needs tailscaleAuthKeySecretArn/],
        [{ dashboardCapability: "example.test/cap/open-workspaces" }, /needs tailscaleAuthKeySecretArn/],
        ...["bad", "example.test/", "https://example.test/cap", "example.test/cap\nX=1", "-bad.test/cap", "example.test/" + "a".repeat(512)]
            .map((capability) => [{ dashboardCapability: capability }, /dashboardCapability must be/]),
        [{ backupSchedule: "daily" }, /backupSchedule needs backupUrl/],
        [{ backupUrl: "s3://my-bucket/ow", backupSchedule: "daily; id" }, /systemd calendar/],
        [{ idleMinutes: 1.5 }, /idleMinutes/],
        ...[0, 3, 1.5, "2", null, true].map((vcpuOvercommit) => [{ vcpuOvercommit }, /vcpuOvercommit/]),
        [{ ref: "a..b" }, /ref must be/],
        [{ repository: "http://plain.example/x" }, /repository must be/],
        [{ developerImages: ["arch"] }, /developerImages takes ubuntu/],
    ]) {
        assert.throws(() => hostConfig({ ...base, ...bad }), message);
    }
});

test("capability admission, store pins and overcommit compose without user data changes", async () => {
    const before = await deploy({});
    const pins = { ubuntu: { version: 1, attempt: "a".repeat(32), digest: "b".repeat(64) } };
    const after = await deploy({ tailscaleAuthKeySecretArn: secretArn, dashboardCapability: "example.test/cap/open-workspaces",
        storeUrl: "s3://bucket/prefix", imageArtifacts: pins, vcpuOvercommit: 2 });
    assert.deepEqual(after.config.dashboard, { logins: [], capability: "example.test/cap/open-workspaces" });
    assert.deepEqual(after.config.store, { url: "s3://bucket/prefix", images: pins });
    assert.equal(after.config.vcpuOvercommit, 2);
    assert.equal(before.userData, after.userData);
});

test("user data keeps values literal and starts with the bootstrap shebang", () => {
    for (const value of [`it's $(touch /tmp/pwned) "quoted"`, "release-$&", "release-$$", "a-$'-$`-$1"]) {
        const script = renderUserData({ OW_CONFIG_PARAMETER: value }, bootstrap);
        assert.ok(script.startsWith("#!/bin/bash\n"));
        assert.ok(!script.includes("# @OW_ENV@"));
        const header = script.split("\n").filter((line) => line.startsWith("OW_"));
        const printed = execFileSync("bash", ["-c", `${header.join("\n")}\nprintf %s "$OW_CONFIG_PARAMETER"`], { encoding: "utf8" });
        assert.equal(printed, value);
    }
    execFileSync("bash", ["-n"], { input: renderUserData({ OW_CONFIG_PARAMETER: "/p" }, bootstrap) });
});

test("stage 0 keeps the tailnet identity on the data volume and hands over to the release", () => {
    const mount = bootstrap.indexOf("mountpoint -q /var/lib/tailscale || mount /var/lib/tailscale");
    assert.ok(bootstrap.indexOf("mountpoint -q /srv/ow || mount /srv/ow") < mount, "data volume first");
    assert.ok(mount > 0 && mount < bootstrap.indexOf("ow_host.py boot"), "state mounted before the release installs tailscale");
    assert.ok(bootstrap.includes("RequiresMountsFor=/var/lib/tailscale"), "tailscaled waits for the state mount at boot");
    assert.ok(bootstrap.includes("chmod 700 /srv/ow/.tailscale-state"));
    assert.ok(!bootstrap.includes("snap install"), "one AWS CLI (v2) only");
    assert.ok(!/set -x|xtrace/.test(bootstrap.replace("Never enable xtrace here.", "")));
});

test("the controller waits for the tailnet before choosing its listen address", () => {
    const wait = file("wait-tailnet");
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

test("dashboard logins and developer images are validated", () => {
    assert.equal(dashboardLoginList(["a@example.com", "b.c+d@example.org"]), "a@example.com,b.c+d@example.org");
    assert.equal(dashboardLoginList([]), "");
    for (const bad of ["no-at-sign", "a@b,c@d", "x@y'", "a b@c", "$(id)@x"]) {
        assert.throws(() => dashboardLoginList([bad]), /invalid dashboard login/);
    }
    assert.equal(developerImageList(["ubuntu", "ubuntu"]), "ubuntu");
    assert.equal(developerImageList([]), "");
    for (const bad of ["alpine", "arch", "ubuntu; id", ""]) {
        assert.throws(() => developerImageList([bad]), /developerImages takes ubuntu/);
    }
    for (const name of ["dashboard-prepare", "developer-images"]) {
        execFileSync("bash", ["-n"], { input: file(name) });
    }
    assert.ok(!file("developer-images").includes("systemctl restart"), "a new image restarts nothing");
    // Build VMs' sockets must fit Linux's limit: their roots stay out of the long release path.
    assert.ok(file("developer-images").includes("ln -s /srv/ow/build/dev-images data/dev-images"));
});

test("backup URLs are checked and the host role can never delete a backup", () => {
    assert.deepEqual(backupLocation("s3://my-bucket/ow/host-1/"), { bucket: "my-bucket", prefix: "ow/host-1" });
    for (const bad of ["s3://my-bucket", "s3://my-bucket/", "https://my-bucket/ow", "s3://My_Bucket/ow", "s3://b/ow",
        "s3://my-bucket/ow/../x", "s3://my-bucket/a//b", "s3://my-bucket/ow'x", "s3://my-bucket/$(id)"]) {
        assert.throws(() => backupLocation(bad), /backupUrl must be/, bad);
    }
    const policy = JSON.parse(backupPolicy("aws", "s3://my-bucket/ow/host-1"));
    assert.deepEqual([...new Set(policy.Statement.flatMap((s) => [s.Action].flat()))].sort(),
        ["s3:AbortMultipartUpload", "s3:GetObject", "s3:ListBucket", "s3:PutObject"]);
    assert.equal(policy.Statement[0].Resource, "arn:aws:s3:::my-bucket");
    assert.deepEqual(policy.Statement[0].Condition, { StringLike: { "s3:prefix": ["ow/host-1/catalog/*", "ow/host-1/snapshots/*", "ow/host-1/runners/*/snapshots/*"] } });
    assert.deepEqual(policy.Statement[1].Resource, ["arn:aws:s3:::my-bucket/ow/host-1/catalog/*", "arn:aws:s3:::my-bucket/ow/host-1/snapshots/*"]);
});

test("controller backup policy can read runner snapshots without runner writes or store access", () => {
    const controller = JSON.parse(backupPolicy("aws", "s3://my-bucket/ow"));
    const runner = JSON.parse(backupPolicy("aws", "s3://my-bucket/ow/runners/alpha", false));
    const matches = (pattern, value) => new RegExp(`^${pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replaceAll("*", ".*")}$`).test(value);
    const permits = (policy, action, key) => policy.Statement.some((s) => {
        const resource = `arn:aws:s3:::my-bucket${action === "s3:ListBucket" ? "" : `/${key}`}`;
        const prefixes = s.Condition?.StringLike?.["s3:prefix"];
        return s.Effect === "Allow" && [s.Action].flat().some((p) => matches(p, action))
            && [s.Resource].flat().some((p) => matches(p, resource))
            && (!prefixes || [prefixes].flat().some((p) => matches(p, key)));
    });
    for (const name of ["alpha", "beta"]) {
        const key = `ow/runners/${name}/snapshots/saved/manifest.json`;
        assert.ok(permits(controller, "s3:ListBucket", key));
        assert.ok(permits(controller, "s3:GetObject", key));
        for (const action of ["s3:PutObject", "s3:AbortMultipartUpload", "s3:DeleteObject", "s3:DeleteObjectVersion"]) {
            assert.ok(!permits(controller, action, key), action);
        }
        assert.equal(permits(runner, "s3:GetObject", key), name === "alpha");
        assert.equal(permits(runner, "s3:ListBucket", key), name === "alpha");
    }
    for (const key of ["ow/store/v1/u/aa/token/manifest.json", "ow/runners/alpha/store/v1/u/aa/token/manifest.json", "other/snapshots/saved/manifest.json"]) {
        for (const policy of [controller, runner]) {
            assert.ok(!permits(policy, "s3:GetObject", key), key);
            assert.ok(!permits(policy, "s3:ListBucket", key), key);
        }
    }
    for (const key of ["ow/catalog/current.sqlite3", "ow/snapshots/saved/manifest.json"]) {
        assert.ok(permits(controller, "s3:GetObject", key));
        assert.ok(permits(controller, "s3:PutObject", key));
        assert.ok(!permits(runner, "s3:GetObject", key));
    }
    assert.ok(permits(runner, "s3:PutObject", "ow/runners/alpha/snapshots/saved/manifest.json"));
});

test("ow-backup uploads the catalog, then the snapshots, and stops at the first failure", () => {
    const dir = mkdtempSync(join(tmpdir(), "ow-backup-"));
    writeFileSync(join(dir, "backup.env"), "OW_BACKUP_URL=s3://my-bucket/ow/host-1\n");
    const backup = file("ow-backup")
        .replace("export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", `export PATH=${dir}:/usr/bin:/bin`)
        .replace("/etc/open-workspaces/backup.env", join(dir, "backup.env"))
        .replaceAll("/srv/ow/.ow/backups", join(dir, "backups"))
        .replace("/run/ow-backup.lock", join(dir, "lock"));
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
    // flock(1) is Linux-only; the lock itself is checked on the host.
    writeFileSync(join(dir, "flock"), "#!/bin/sh\nexit 0\n", { mode: 0o755 });
    assert.ok(backup.includes('flock -n 9 || { echo "ow-backup is already running" >&2; exit 1; }'), "one run at a time");
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
