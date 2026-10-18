import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const bin = fileURLToPath(new URL("../bin/", import.meta.url));
const helper = join(bin, "ow-aws");
const remoteLib = join(bin, "ow-aws-remote.sh");
const INSTANCE = "i-0123456789abcdef0";

test("the helper and its host functions pass shellcheck", { skip: spawnSync("shellcheck", ["--version"]).status !== 0 && "no shellcheck" }, () => {
    execFileSync("shellcheck", ["-x", helper, remoteLib], { stdio: "pipe" });
});

// Stand-ins for aws, pulumi and sleep. Each SSM command's script is decoded and
// kept; its status and output come from `results`, one per command in order.
// With `hostBin`, each script really runs in bash, with hostBin first on PATH,
// and its exit status and output become the command's result.
// `instances` maps tag names to instance IDs (default: every name is INSTANCE); `parameter`
// is what `aws ssm get-parameter` prints (a host's config parameter); `stackInstance` is the
// stack's instanceId output (default INSTANCE).
function fake({ state = "running", results = [], preview = { steps: [] }, hostBin = null, instances = null, parameter = "", stackInstance = INSTANCE } = {}) {
    const dir = mkdtempSync(join(tmpdir(), "ow-aws-"));
    mkdirSync(join(dir, "bin"));
    mkdirSync(join(dir, "scripts"));
    writeFileSync(join(dir, "config.json"), JSON.stringify({ state, results, preview, hostBin, instances, parameter, stackInstance }));
    writeFileSync(join(dir, "bin", "aws"), `#!/usr/bin/env node
const fs = require("fs"), path = require("path");
const dir = ${JSON.stringify(dir)}, args = process.argv.slice(2);
const config = JSON.parse(fs.readFileSync(path.join(dir, "config.json"), "utf8"));
fs.appendFileSync(path.join(dir, "calls.log"), JSON.stringify(args) + "\\n");
const arg = (name) => args[args.indexOf(name) + 1];
const out = (text) => process.stdout.write(text + "\\n");
const sent = () => fs.readdirSync(path.join(dir, "scripts")).length;
const tag = (args.find((a) => a.startsWith("Name=tag:Name,Values=")) || "").split("=").pop();
const nameOf = (id) => Object.entries(config.instances || {}).find(([, v]) => v === id)?.[0] ?? (id === ${JSON.stringify(INSTANCE)} ? "ow-test" : "other");
if (args[1] === "describe-instances" && arg("--query").includes("Tags")) out(nameOf(arg("--instance-ids")));
else if (args[1] === "describe-instances") out(arg("--query").includes("State") ? config.state : (config.instances || {})[tag] || ${JSON.stringify(INSTANCE)});
else if (args[1] === "get-parameter") { if (config.parameter) out(config.parameter); else process.exit(254); }
else if (args[1] === "describe-instance-information") out("Online");
else if (args[1] === "send-command") {
    const parameters = JSON.parse(arg("--parameters"));
    const script = Buffer.from(parameters.commands[0].split(" ")[1], "base64").toString();
    const n = sent();
    fs.writeFileSync(path.join(dir, "scripts", n + ".json"), JSON.stringify({ script, timeout: parameters.executionTimeout[0], instance: arg("--instance-ids") }));
    if (config.hostBin) {
        const run = require("child_process").spawnSync("bash", ["-c", script], { encoding: "utf8", env: { PATH: config.hostBin + ":" + process.env.PATH } });
        config.results[n] = { status: run.status === 0 ? "Success" : "Failed", stdout: run.stdout + run.stderr };
        fs.writeFileSync(path.join(dir, "config.json"), JSON.stringify(config));
    }
    out("cmd-" + n);
} else if (args[1] === "get-command-invocation") {
    const result = config.results[Number(arg("--command-id").slice(4))] || {};
    const query = arg("--query");
    out(query === "Status" ? result.status || "Success" : query === "StandardOutputContent" ? result.stdout || "" : "");
}
`, { mode: 0o755 });
    writeFileSync(join(dir, "bin", "pulumi"), `#!/usr/bin/env node
const config = JSON.parse(require("fs").readFileSync(${JSON.stringify(join(dir, "config.json"))}, "utf8"));
const args = process.argv.slice(2);
// The streaming format: one engine event per line instead of one document.
if (args[0] === "preview" && process.env.PULUMI_ENABLE_STREAMING_JSON_PREVIEW) {
    for (const step of config.preview.steps || []) console.log(JSON.stringify({ resourcePreEvent: { metadata: step } }));
    console.log(JSON.stringify({ summaryEvent: {} }));
} else if (args[0] === "preview") console.log(typeof config.preview === "string" ? config.preview : JSON.stringify(config.preview));
else if (args[0] === "stack" && args[2] === "instanceId") console.log(config.stackInstance);
else process.exit(9);
`, { mode: 0o755 });
    writeFileSync(join(dir, "bin", "sleep"), "#!/bin/sh\n", { mode: 0o755 });
    const run = (args, env = {}) => spawnSync(helper, args, {
        encoding: "utf8",
        env: { PATH: `${join(dir, "bin")}:${process.env.PATH}`, HOME: dir, OW_HOST: "ow-test", ...env },
    });
    const calls = () => existsSync(join(dir, "calls.log"))
        ? readFileSync(join(dir, "calls.log"), "utf8").trim().split("\n").map((line) => JSON.parse(line))
        : [];
    const scripts = () => readdirSync(join(dir, "scripts")).sort((a, b) => parseInt(a) - parseInt(b))
        .map((name) => JSON.parse(readFileSync(join(dir, "scripts", name), "utf8")));
    const lastLine = (script) => script.trimEnd().split("\n").at(-1).trim();
    return { dir, run, calls, scripts, lastLine, done: () => rmSync(dir, { recursive: true, force: true }) };
}

test("each command sends the host functions and one quoted call, and every script parses", () => {
    const f = fake({ results: [{ stdout: "hi" }] });
    const result = f.run(["run", "echo 'it''s' $HOME; uptime"]);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, "hi\n");
    const [{ script, timeout }] = f.scripts();
    assert.equal(timeout, "600");
    assert.ok(script.startsWith(readFileSync(remoteLib, "utf8")));
    // The call runs the commands verbatim in a child bash, never expanded by the helper.
    const call = f.lastLine(script);
    const argv = execFileSync("bash", ["-c", `printf '%s\\n' ${call}`], { encoding: "utf8" }).trim().split("\n");
    assert.deepEqual(argv, ["bash", "-euo", "pipefail", "-c", "echo 'it''s' $HOME; uptime"]);
    execFileSync("bash", ["-n", "-c", script]);
    assert.deepEqual(f.calls()[0].slice(0, 3), ["ec2", "describe-instances", "--filters"]);
    assert.ok(f.calls()[0].includes("Name=tag:Name,Values=ow-test"));
    f.done();
});

test("stop hibernates the boxes first and never stops on a failure or an unknown state", () => {
    let f = fake({ state: "running" });
    assert.equal(f.run(["stop"]).status, 0);
    assert.equal(f.lastLine(f.scripts()[0].script), "hibernate_boxes");
    const ops = f.calls().map((c) => c[1]);
    assert.ok(ops.indexOf("send-command") < ops.indexOf("stop-instances"), ops.join(" "));
    f.done();

    f = fake({ state: "running", results: [{ status: "Failed", stdout: "box a: hibernate failed" }] });
    assert.notEqual(f.run(["stop"]).status, 0);
    assert.ok(!f.calls().some((c) => c[1] === "stop-instances"), "stopped with boxes not hibernated");
    f.done();

    f = fake({ state: "stopping" });
    const result = f.run(["stop"]);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /is stopping/);
    assert.ok(!f.calls().some((c) => c[1] === "send-command" || c[1] === "stop-instances"));
    f.done();
});

test("stop never stops the instance when the host cannot list its boxes", () => {
    const hostBin = mkdtempSync(join(tmpdir(), "ow-aws-hostbin-"));
    writeFileSync(join(hostBin, "ow"), '#!/bin/sh\n[ "$1" = list ] && { echo "worker unavailable" >&2; exit 1; }\nexit 0\n', { mode: 0o755 });
    const f = fake({ state: "running", hostBin });
    const result = f.run(["stop"]);
    assert.notEqual(result.status, 0, result.stdout);
    assert.ok(!f.calls().some((c) => c[1] === "stop-instances"), "stopped with an unknown set of running boxes");
    f.done();
    rmSync(hostBin, { recursive: true, force: true });
});

test("start waits for SSM, then readiness, then resumes the remembered boxes", () => {
    const f = fake({ state: "stopped" });
    assert.equal(f.run(["start"]).status, 0);
    const ops = f.calls().map((c) => c.slice(0, 2).join(" "));
    assert.ok(ops.indexOf("ssm describe-instance-information") < ops.indexOf("ssm send-command"));
    assert.equal(f.lastLine(f.scripts()[0].script), "after_start 600");
    f.done();
});

test("ci-preflight hibernates only when the preview replaces this host's instance", () => {
    const urn = (name) => `urn:pulumi:rain::proj::open-workspaces-aws:index:Host$aws:ec2/instance:Instance::${name}`;
    for (const [steps, hibernates] of [
        [[{ op: "replace", urn: urn("ow-test-host") }], true],
        [[{ op: "create-replacement", urn: urn("ow-test-host") }], true],
        [[{ op: "update", urn: urn("ow-test-host") }], false],
        [[{ op: "replace", urn: urn("ow-test2-host") }], false],
        [[{ op: "replace", urn: urn("ow-test-host").replace("instance:Instance", "volumeAttachment:VolumeAttachment") }], false],
        [[], false],
    ]) {
        const f = fake({ preview: { steps } });
        const result = f.run(["ci-preflight", "--stack", "rain"]);
        assert.equal(result.status, 0, result.stderr);
        assert.deepEqual(f.scripts().map((s) => f.lastLine(s.script)), hibernates ? ["hibernate_for_replacement"] : [], JSON.stringify(steps));
        f.done();
    }
    let f = fake({ state: "pending", preview: { steps: [{ op: "replace", urn: urn("ow-test-host") }] } });
    assert.notEqual(f.run(["ci-preflight", "--stack", "rain"]).status, 0, "replaced on a guess");
    assert.equal(f.scripts().length, 0);
    f.done();

    // An unreadable preview is never taken as "kept".
    const invalidSteps = [false, 0, "", {}, null, "replace"].map((steps) => JSON.stringify({ steps, changeSummary: { replace: 1 } }));
    for (const preview of ["{not json", "[]", "{}", "", ...invalidSteps, JSON.stringify({ steps: [1] })]) {
        f = fake({ preview });
        const result = f.run(["ci-preflight", "--stack", "rain"]);
        assert.notEqual(result.status, 0, JSON.stringify(preview));
        assert.match(result.stderr, /could not read the pulumi preview/);
        f.done();
    }
    // A preview with no steps key and a summary is a preview that changes nothing.
    f = fake({ preview: { changeSummary: { same: 12 } } });
    assert.equal(f.run(["ci-preflight", "--stack", "rain"]).status, 0);
    assert.equal(f.scripts().length, 0);
    f.done();
    // The streaming setting is turned off for this preview, so a replacement is still seen.
    f = fake({ preview: { steps: [{ op: "replace", urn: urn("ow-test-host") }] } });
    const result = f.run(["ci-preflight", "--stack", "rain"], { PULUMI_ENABLE_STREAMING_JSON_PREVIEW: "true" });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(f.scripts().map((s) => f.lastLine(s.script)), ["hibernate_for_replacement"]);
    f.done();
});

test("ci-post-up starts a stopped host, waits, then builds each recipe", () => {
    const f = fake({ state: "stopped" });
    const recipe = join(f.dir, "team.sh");
    writeFileSync(recipe, "apt-get install -y jq\n");
    const result = f.run(["ci-post-up", "--stack", "rain", "--image", recipe, "--max-class", "1"]);
    assert.equal(result.status, 0, result.stderr);
    assert.ok(f.calls().some((c) => c[1] === "start-instances"));
    const [post, build] = f.scripts();
    assert.equal(f.lastLine(post.script), "post_up 3000 --max-class 1");
    assert.equal(f.lastLine(build.script), `build_image team.sh ${Buffer.from("apt-get install -y jq\n").toString("base64")}`);
    assert.equal(build.timeout, "3600");
    f.done();
});

test("ssh-config pins the host and box keys and checks them strictly", () => {
    const hostKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHostKeyHostKeyHostKeyHostKeyHostKey";
    const boxKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBoxKeyBoxKeyBoxKeyBoxKeyBoxKeyBox";
    let f = fake({ results: [{ stdout: hostKey }, { stdout: JSON.stringify({ identity: "m-1", host_key: boxKey }) }] });
    const config = join(f.dir, "ow.conf");
    const result = f.run(["ssh-config", "dev", "-o", config]);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(readFileSync(`${config}.known_hosts`, "utf8"), `ow-test ${hostKey}\now-m-1 ${boxKey}\n`);
    const text = readFileSync(config, "utf8");
    assert.equal(text.match(/StrictHostKeyChecking yes/g).length, 2);
    assert.equal(text.match(new RegExp(`UserKnownHostsFile ${config}.known_hosts`, "g")).length, 2);
    assert.match(text, /Host ow-test\n {4}HostName i-0123456789abcdef0\n {4}HostKeyAlias ow-test\n/);
    assert.match(text, /Host ow-dev\n[^]*HostKeyAlias ow-m-1\n[^]*ProxyCommand ssh -F \S+ ow-test sudo ow ssh-proxy %h$/m);
    assert.ok(!/StrictHostKeyChecking (no|ask)/.test(text));
    assert.equal(f.lastLine(f.scripts()[1].script), "ow ssh-info dev");
    f.done();

    // A key the host should not have sent writes nothing.
    f = fake({ results: [{ stdout: hostKey }, { stdout: JSON.stringify({ identity: "m-1", host_key: "ssh-rsa AAAA\nHost *" }) }] });
    assert.notEqual(f.run(["ssh-config", "dev", "-o", join(f.dir, "ow.conf")]).status, 0);
    assert.ok(!existsSync(join(f.dir, "ow.conf")) && !existsSync(join(f.dir, "ow.conf.known_hosts")));
    f.done();
});

test("inputs are checked before anything reaches AWS", () => {
    const f = fake();
    mkdirSync(join(f.dir, ".ssh"));
    writeFileSync(join(f.dir, ".ssh", "id_ed25519.pub"), "ssh-rsa AAAAB3Nza you@example.test\n");
    writeFileSync(join(f.dir, "Bad_Name.sh"), "true\n");
    for (const args of [
        ["authorize", "dev"],
        ["authorize", "dev;reboot"],
        ["ssh-config", "../x"],
        ["join-tailnet", "--secret", "ow/tailscale"],
        ["image", join(f.dir, "Bad_Name.sh")],
        ["upgrade", "v1; reboot"],
        ["backups", "--url", "https://example.test/x"],
        ["restart", "--everything"],
        ["ci-preflight"],
        ["ci-post-up", "--stack", "rain", "--image", join(f.dir, "Bad_Name.sh")],
    ]) {
        const result = f.run(args);
        assert.notEqual(result.status, 0, args.join(" "));
    }
    assert.notEqual(f.run(["run", "true"], { OW_HOST: "Bad Host" }).status, 0);
    assert.deepEqual(f.calls(), []);
    f.done();
});

test("join-tailnet passes the secret ARN and the host name, never a key", () => {
    const f = fake();
    const arn = "arn:aws:secretsmanager:us-east-1:123456789012:secret:ow/tailscale-AbCdEf";
    assert.equal(f.run(["join-tailnet", "--secret", arn]).status, 0);
    assert.equal(f.lastLine(f.scripts()[0].script), `join_tailnet ${arn} ow-test`);
    f.done();
});

// The host functions, run here with stand-ins for ow, systemctl and the rest.
function host(body, { list = [], failStart = "", backups = "[]" } = {}) {
    const dir = mkdtempSync(join(tmpdir(), "ow-aws-host-"));
    mkdirSync(join(dir, "bin"));
    writeFileSync(join(dir, "list.json"), JSON.stringify(list));
    writeFileSync(join(dir, "backups.json"), backups);
    writeFileSync(join(dir, "bin", "ow"), `#!/bin/sh
echo "ow $*" >> "${dir}/log"
case $1 in
  list) [ -z "$LIST_FAIL" ] || { echo "worker unavailable" >&2; exit 1; }; cat "${dir}/list.json" ;;
  hibernate) [ "$2" != "$FAIL" ] ;;
  start) [ "$2" != "${failStart}" ] ;;
  "node") echo '[{"online": true}]' ;;
  backup) cat "${dir}/backups.json" ;;
esac
`, { mode: 0o755 });
    for (const name of ["systemctl", "id", "tail", "journalctl"]) writeFileSync(join(dir, "bin", name), "#!/bin/sh\n", { mode: 0o755 });
    writeFileSync(join(dir, "bin", "timeout"), '#!/bin/sh\nshift\nexec "$@"\n', { mode: 0o755 });
    writeFileSync(join(dir, "bin", "cloud-init"), '#!/bin/sh\nexit "${CLOUD_INIT:-0}"\n', { mode: 0o755 });
    const script = `. ${JSON.stringify(remoteLib)}
resume_list=${dir}/resume bootstrap_status=${dir}/status
${body}`;
    const result = spawnSync("bash", ["-c", script], { encoding: "utf8", env: { PATH: `${join(dir, "bin")}:${process.env.PATH}` } });
    const read = (name) => (existsSync(join(dir, name)) ? readFileSync(join(dir, name), "utf8") : null);
    const out = { ...result, resume: read("resume"), log: read("log") ?? "" };
    rmSync(dir, { recursive: true, force: true });
    return out;
}

const boxes = [{ id: "a", state: "running" }, { id: "b", state: "hibernated" }, { id: "c", state: "running" }];

test("hibernate_boxes remembers each box it hibernated and stops at the first failure", () => {
    let result = host("hibernate_boxes", { list: boxes });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.resume, "a\nc\n");
    assert.match(result.log, /ow hibernate a\now hibernate c/);
    result = host("FAIL=c; export FAIL; hibernate_boxes", { list: boxes });
    assert.notEqual(result.status, 0);
    assert.equal(result.resume, "a\n", "a box that did not hibernate is not remembered");
    result = host("LIST_FAIL=1; export LIST_FAIL; hibernate_boxes; echo after", { list: boxes });
    assert.notEqual(result.status, 0, "an unreadable box list read as no running boxes");
    assert.ok(!result.stdout.includes("after"));
});

test("resume_boxes starts remembered boxes and keeps the ones that failed", () => {
    const list = [{ id: "a", state: "hibernated" }, { id: "c", state: "running" }];
    let result = host("printf 'a\\nc\\ngone\\na\\n' > \"$resume_list\"; resume_boxes", { list });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.resume, null);
    assert.match(result.stdout, /resumed a\nc is already running\ngone no longer exists; dropped/);
    assert.equal(result.log.match(/ow start/g).length, 1);
    result = host("printf 'a\\n' > \"$resume_list\"; resume_boxes", { list, failStart: "a" });
    assert.notEqual(result.status, 0);
    assert.equal(result.resume, "a\n");
    assert.equal(host("resume_boxes").status, 0, "nothing to resume");
});

test("bootstrap_done needs cloud-init done and the bootstrap's ok status", () => {
    assert.equal(host("echo 'ok tailscale=joined' > \"$bootstrap_status\"; CLOUD_INIT=2 bootstrap_done 5").status, 0);
    for (const [status, cloudInit] of [["failed", 0], ["running", 0], ["ok tailscale=skipped", 1], [null, 0]]) {
        const write = status ? `echo '${status}' > "$bootstrap_status"; ` : "";
        const result = host(`${write}CLOUD_INIT=${cloudInit} bootstrap_done 5`);
        assert.notEqual(result.status, 0, `${status} ${cloudInit}`);
    }
});

test("backups lists captures by owner and machine, oldest first, and filters by box", () => {
    const backups = JSON.stringify([
        { snapshot: "sleep-1", workspace: "m-gone", owner: null, machine: null, uploaded: "2026-10-15 01:00:00" },
        { snapshot: "sleep-2", workspace: "m-1", owner: "a@example.test", machine: "dev", uploaded: "2026-10-16 01:00:00" },
    ]);
    let result = host("backups s3://b/ow-host ''", { backups });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, "2026-10-15 01:00:00  -  m-gone  sleep-1\n2026-10-16 01:00:00  a@example.test/dev  m-1  sleep-2\n");
    assert.match(result.log, /ow backup list s3:\/\/b\/ow-host\/snapshots/);
    assert.equal(host("backups s3://b/ow-host/ a@example.test/dev", { backups }).stdout, "2026-10-16 01:00:00  a@example.test/dev  m-1  sleep-2\n");
    assert.equal(host("backups s3://b/p m-gone", { backups }).stdout, "2026-10-15 01:00:00  -  m-gone  sleep-1\n");
    // Hosts before this release list names only.
    assert.equal(host("backups s3://b/p ''", { backups: '["sleep-1"]' }).stdout, "-  -  -  sleep-1\n");
});

const RUNNER_CONFIG = JSON.stringify({ schema: 1, role: "runner", runner: { node: "ow-big", controller: "ow-test", joinSecretArn: "arn" } });
const CONTROLLER_ID = "i-0aaaaaaaaaaaaaaa1", RUNNER_ID = "i-0bbbbbbbbbbbbbbb2";

test("runner-add mints on the controller and enrols on the runner; the join never passes through here", () => {
    const status = JSON.stringify({ role: "runner", node: null, budget: { memory: 49152, slots: 8, cpus: 16 } });
    const f = fake({ instances: { "ow-test": CONTROLLER_ID, "ow-big": RUNNER_ID }, results: [{}, { stdout: status }] });
    const result = f.run(["runner-add", "ow-big"]);
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(f.scripts().map((s) => [s.instance, f.lastLine(s.script)]), [
        [RUNNER_ID, "ready 3000 ''"],
        [RUNNER_ID, "ow_host status"],
        [CONTROLLER_ID, "ow_host invite-runner ow-big --memory 49152 --slots 8 --cpus 16"],
        [RUNNER_ID, "ow_host enrol --wait 300"],
        [CONTROLLER_ID, "node_online ow-big 120"],
    ]);
    assert.ok(!f.calls().some((c) => c.includes("secretsmanager")), "this machine never reads or writes the join");
    f.done();
});

test("runner-add on an enrolled runner only checks that it is online, and refuses a non-runner", () => {
    let f = fake({ results: [{}, { stdout: JSON.stringify({ role: "runner", node: "ow-big", budget: { memory: 1024, slots: 2, cpus: 2 } }) }] });
    assert.equal(f.run(["runner-add", "ow-big"]).status, 0);
    assert.deepEqual(f.scripts().map((s) => f.lastLine(s.script)), ["ready 3000 ''", "ow_host status", "node_online ow-big 120"]);
    f.done();
    f = fake({ results: [{}, { stdout: JSON.stringify({ role: "host", node: "ow-big", budget: { memory: 1024, slots: 2, cpus: 2 } }) }] });
    const result = f.run(["runner-add", "ow-big"]);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /not a runner host/);
    assert.equal(f.scripts().length, 2, "nothing minted");
    f.done();
    f = fake();
    for (const args of [["runner-add"], ["runner-add", "ow-test"], ["runner-add", "a b"], ["runner-remove", "x;y"]]) {
        assert.notEqual(f.run(args).status, 0, args.join(" "));
    }
    assert.equal(f.scripts().length, 0);
    f.done();
});

test("a runner upgrades only to an agent its controller accepts: the controller's release is passed", () => {
    const f = fake({ parameter: RUNNER_CONFIG, instances: { "ow-test": CONTROLLER_ID, "ow-big": RUNNER_ID },
        results: [{ stdout: JSON.stringify({ versions: { agent: "0.6.0" } }) }] });
    const result = f.run(["upgrade", "--max-class", "1"], { OW_HOST: "ow-big" });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(f.scripts().map((s) => [s.instance, f.lastLine(s.script)]), [
        [CONTROLLER_ID, "ow_host status"],
        [RUNNER_ID, "ow_host upgrade --max-class 1 --controller-version 0.6.0"],
    ]);
    f.done();
});

test("ci-post-up on a runner: ready, enrolled with its controller, then apply and upgrade", () => {
    const status = JSON.stringify({ role: "runner", node: "ow-big", budget: { memory: 1024, slots: 2, cpus: 2 } });
    const f = fake({ parameter: RUNNER_CONFIG, instances: { "ow-test": CONTROLLER_ID, "ow-big": RUNNER_ID },
        results: [{}, {}, { stdout: status }, {}, {}, { stdout: JSON.stringify({ versions: { agent: "0.6.0" } }) }] });
    const result = f.run(["ci-post-up", "--stack", "s"], { OW_HOST: "ow-big", OW_INSTANCE: RUNNER_ID });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(f.scripts().map((s) => [s.instance, f.lastLine(s.script)]), [
        [RUNNER_ID, "after_start 3000"],
        [RUNNER_ID, "ready 3000 ''"],
        [RUNNER_ID, "ow_host status"],
        [CONTROLLER_ID, "node_online ow-big 120"],
        [RUNNER_ID, "ow_host apply"],
        [CONTROLLER_ID, "ow_host status"],
        [RUNNER_ID, "ow_host upgrade --controller-version 0.6.0"],
    ]);
    f.done();
});

test("a host with no config parameter (older than 0.5.0) still takes the host path", () => {
    const f = fake();
    assert.equal(f.run(["ci-post-up", "--stack", "s"]).status, 0);
    assert.deepEqual(f.scripts().map((s) => f.lastLine(s.script)), ["post_up 3000"]);
    f.done();
});

test("node_online waits for the node, and runner_remove never revokes an online runner", () => {
    const dir = mkdtempSync(join(tmpdir(), "ow-node-"));
    const list = join(dir, "list.json");
    writeFileSync(join(dir, "ow"), `#!/bin/sh\n[ "$1 $2" = "node list" ] && exec cat ${list}\necho "$@" >> ${join(dir, "revoked")}\n`, { mode: 0o755 });
    writeFileSync(join(dir, "sleep"), "#!/bin/sh\n", { mode: 0o755 });
    const host = (call) => spawnSync("bash", ["-c", `. ${remoteLib}; ${call}`], { encoding: "utf8", env: { PATH: `${dir}:${process.env.PATH}` } });
    writeFileSync(list, JSON.stringify([{ id: "ow-big", online: true }]));
    assert.equal(host("node_online ow-big 5").status, 0);
    const refused = host("runner_remove ow-big");
    assert.notEqual(refused.status, 0);
    assert.match(refused.stdout, /is online/);
    assert.ok(!existsSync(join(dir, "revoked")));
    writeFileSync(list, JSON.stringify([{ id: "ow-big", online: false }]));
    assert.notEqual(host("node_online ow-big 0").status, 0);
    assert.equal(host("runner_remove ow-big").status, 0);
    assert.equal(readFileSync(join(dir, "revoked"), "utf8"), "--data-dir /srv/ow/.ow/controller node-revoke ow-big\n");
    rmSync(dir, { recursive: true });
});

test("a runner's backups are listed from its controller, under the runner's own prefix", () => {
    const parameter = JSON.stringify({ ...JSON.parse(RUNNER_CONFIG), backup: { url: "s3://my-bucket/ow/runners/ow-big", schedule: "" } });
    const f = fake({ parameter, instances: { "ow-test": CONTROLLER_ID, "ow-big": RUNNER_ID } });
    assert.equal(f.run(["backups", "ow-big-box"], { OW_HOST: "ow-big" }).status, 0);
    assert.deepEqual(f.scripts().map((s) => [s.instance, f.lastLine(s.script)]),
        [[CONTROLLER_ID, "backups s3://my-bucket/ow/runners/ow-big ow-big-box"]]);
    f.done();
});

test("in a stack whose instanceId is the controller, runner CI commands act on the runner only", () => {
    const instances = { "ow-test": CONTROLLER_ID, "ow-big": RUNNER_ID };
    const status = JSON.stringify({ role: "runner", node: "ow-big", budget: { memory: 1024, slots: 2, cpus: 2 } });
    let f = fake({ parameter: RUNNER_CONFIG, instances, stackInstance: CONTROLLER_ID,
        results: [{}, {}, { stdout: status }, {}, {}, { stdout: JSON.stringify({ versions: { agent: "0.6.0" } }) }] });
    let result = f.run(["ci-post-up", "--stack", "s"], { OW_HOST: "ow-big" });
    assert.equal(result.status, 0, result.stderr);
    const sent = f.scripts().map((s) => [s.instance, f.lastLine(s.script)]);
    assert.deepEqual(sent.filter(([id]) => id === CONTROLLER_ID).map(([, call]) => call),
        ["node_online ow-big 120", "ow_host status"], "the controller is only asked, never changed");
    assert.deepEqual(sent.filter(([id]) => id === RUNNER_ID).map(([, call]) => call).filter((c) => !c.startsWith("ready") && c !== "ow_host status"),
        ["after_start 3000", "ow_host apply", "ow_host upgrade --controller-version 0.6.0"]);
    assert.ok(!f.calls().some((c) => c[1] === "start-instances" || c[1] === "stop-instances"));
    f.done();
    const urn = "urn:pulumi:s::p::open-workspaces-aws:index:Runner$aws:ec2/instance:Instance::ow-big-host";
    f = fake({ instances, stackInstance: CONTROLLER_ID, preview: { steps: [{ op: "replace", urn }] } });
    result = f.run(["ci-preflight", "--stack", "s"], { OW_HOST: "ow-big" });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(f.scripts().map((s) => [s.instance, f.lastLine(s.script)]), [[RUNNER_ID, "hibernate_for_replacement"]]);
    f.done();
    // The Host itself still takes the stack's instance (its own), with no tag lookup.
    f = fake({ instances, stackInstance: CONTROLLER_ID, preview: { steps: [{ op: "replace", urn: urn.replace("Runner", "Host").replace("ow-big", "ow-test") }] } });
    assert.equal(f.run(["ci-preflight", "--stack", "s"]).status, 0);
    assert.deepEqual(f.scripts().map((s) => s.instance), [CONTROLLER_ID]);
    assert.ok(!f.calls().some((c) => c[1] === "describe-instances" && c.some((a) => a.startsWith("Name=tag:Name"))));
    f.done();
});
