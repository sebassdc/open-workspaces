import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import * as pulumi from "@pulumi/pulumi";

// As userdata.test.mjs: CommonJS globals, then mocks before the component loads.
globalThis.__dirname = fileURLToPath(new URL("..", import.meta.url));
globalThis.require = createRequire(new URL("../index.ts", import.meta.url));

const created = [];
pulumi.runtime.setMocks({
    newResource: (args) => {
        created.push(args);
        return { id: `${args.name}-id`, state: { name: args.name, macAddress: "02:00:00:00:00:01", ...args.inputs, arn: `arn:aws:mock:::${args.name}` } };
    },
    call: (args) => ({
        "aws:ec2/getVpc:getVpc": { id: "vpc-1" },
        "aws:ec2/getInstanceTypeOfferings:getInstanceTypeOfferings": { locations: ["us-east-1a"] },
        "aws:ec2/getSubnets:getSubnets": { ids: ["subnet-1"] },
        "aws:ec2/getSubnet:getSubnet": { id: args.inputs.id ?? "subnet-1", vpcId: "vpc-1", availabilityZone: "us-east-1a" },
        "aws:index/getPartition:getPartition": { partition: "aws" },
        "aws:index/getRegion:getRegion": { region: "us-east-1", name: "us-east-1" },
        "aws:ssm/getParameter:getParameter": { value: "ami-1" },
    })[args.token] ?? {},
}, "project", "stack", false);
const { Host, Runner, hostConfig, nodeId, tailscaleDirectSubnet } = await import("../index.ts");

const settle = () => new Promise((resolve) => setTimeout(resolve, 200));
const secretArn = "arn:aws:secretsmanager:us-east-1:111122223333:secret:ow/ts-AbCdEf";
const hostArgs = { tailscaleAuthKeySecretArn: secretArn, dashboardLogins: ["you@example.com"], backupUrl: "s3://my-bucket/ow" };
const byName = (name, type = "") => created.find((r) => r.name === name && r.type.includes(type));
const policy = (name) => JSON.parse(byName(name, "rolePolicy:").inputs.policy).Statement;

/** Every resource as "type name" -> inputs, for comparing two deployments. */
function resources() {
    return new Map(created.map((r) => [`${r.type} ${r.name}`, r.inputs]));
}

test("adding a runner leaves every resource of its controller host unchanged", async () => {
    created.length = 0;
    new Host("ow-ctl", hostArgs);
    await settle();
    const alone = resources();
    created.length = 0;
    const host = new Host("ow-ctl", hostArgs);
    new Runner("ow-big", { controller: host, tailscaleAuthKeySecretArn: secretArn, instanceType: "m8i.2xlarge",
        backupUrl: "s3://my-bucket/ow", developerImages: ["ubuntu"] });
    await settle();
    const both = resources();
    for (const [name, resource] of alone) assert.deepEqual(both.get(name), resource, name);
    const added = [...both.keys()].filter((name) => !alone.has(name));
    assert.ok(added.every((key) => / ow-big(-|$)/.test(key)), added.join(", "));
    // The one resource that names the controller: its role may write this runner's join, nothing else.
    const touching = added.filter((key) => JSON.stringify(both.get(key)).includes("ow-ctl-"));
    assert.deepEqual(touching, ["aws:iam/rolePolicy:RolePolicy ow-big-invite"]);
    assert.equal(byName("ow-big-invite").inputs.role, "ow-ctl-host", "the controller's role name");
    assert.deepEqual(policy("ow-big-invite"),
        [{ Effect: "Allow", Action: "secretsmanager:PutSecretValue", Resource: "arn:aws:mock:::ow-big-join" }]);
});

test("a runner has its own instance, volume, role, join secret and backup prefix", async () => {
    created.length = 0;
    const host = new Host("ow-ctl", hostArgs);
    new Runner("ow-big", { controller: host, tailscaleAuthKeySecretArn: secretArn, instanceType: "m8i.2xlarge",
        backupUrl: "s3://my-bucket/ow/", backupSchedule: "*-*-* 02:00:00 UTC", developerImages: ["ubuntu"] });
    await settle();
    const instance = byName("ow-big-host", "ec2/instance:");
    assert.equal(instance.inputs.instanceType, "m8i.2xlarge");
    assert.deepEqual(instance.inputs.cpuOptions, { nestedVirtualization: "enabled" });
    assert.equal(instance.inputs.iamInstanceProfile, "ow-big-host");
    assert.ok(byName("ow-big-data", "ebs/volume:"), "its own data volume");
    assert.equal(byName("ow-big-host", "securityGroup:").inputs.ingress, undefined, "no inbound rules");
    // Stage 0 is the same as a host's: the role lives in the config parameter.
    const ctlData = byName("ow-ctl-host", "ec2/instance:").inputs.userData;
    assert.equal(instance.inputs.userData.replace(/ow-big/g, "X").replace(/ow-ctl/g, "X"),
        ctlData.replace(/ow-big/g, "X").replace(/ow-ctl/g, "X"));
    const config = JSON.parse(byName("ow-big-config", "ssm/parameter:").inputs.insecureValue);
    assert.deepEqual(config.runner, { node: "ow-big", controller: "ow-ctl", joinSecretArn: "arn:aws:mock:::ow-big-join" });
    assert.equal(config.role, "runner");
    assert.deepEqual(config.dashboard, { logins: [] });
    assert.deepEqual(config.backup, { url: "s3://my-bucket/ow/runners/ow-big", schedule: "*-*-* 02:00:00 UTC" });
    assert.deepEqual(config.developerImages, ["ubuntu"]);
    assert.equal(config.tailscale.hostname, "ow-big");
    const join = byName("ow-big-join", "secretsmanager/secret:");
    assert.equal(join.inputs.name, "open-workspaces/ow-big/join", "the name ow-host invite-runner writes to");
    assert.equal(join.inputs.recoveryWindowInDays, 0, "a destroyed runner can come back under its name");
    assert.ok(!created.some((r) => r.type.includes("secretVersion")), "Pulumi never holds a join");
    assert.equal(byName("ow-big-join", "rolePolicy:").inputs.role, "ow-big-host-id");
    assert.deepEqual(policy("ow-big-join"), [{ Effect: "Allow",
        Action: ["secretsmanager:GetSecretValue", "secretsmanager:PutSecretValue"], Resource: "arn:aws:mock:::ow-big-join" }]);
    const backup = policy("ow-big-backup");
    assert.equal(backup[1].Resource, "arn:aws:s3:::my-bucket/ow/runners/ow-big/*", "its own prefix only");
    assert.deepEqual(backup[0].Condition, { StringLike: { "s3:prefix": ["ow/runners/ow-big/*"] } });
    assert.equal(policy("ow-ctl-backup")[1].Resource, "arn:aws:s3:::my-bucket/ow/*");
});

test("a runner's name is its node ID, and it needs the tailnet and no dashboard", () => {
    for (const bad of ["local", "has space", "x".repeat(33), ""]) assert.throws(() => nodeId(bad), /node ID/, bad);
    assert.throws(() => new Runner("bad name", { controller: {} }), /node ID/);
    const base = { repository: "https://github.com/x/y.git", ref: "v1", tailscaleAuthKeySecretArn: secretArn, tailscaleHostname: "h",
        tailscaleSsh: false, dashboardLogins: [], developerImages: [], backupUrl: "", backupSchedule: "", idleMinutes: 30,
        runner: { node: "ow-big", controller: "ow-ctl", joinSecretArn: "arn:aws:secretsmanager:us-east-1:1:secret:j" } };
    assert.equal(hostConfig(base).role, "runner");
    assert.throws(() => hostConfig({ ...base, tailscaleAuthKeySecretArn: "" }), /needs tailscaleAuthKeySecretArn/);
    assert.throws(() => hostConfig({ ...base, dashboardLogins: ["you@example.com"] }), /no dashboard/);
    assert.throws(() => hostConfig({ ...base, dashboardCapability: "example.test/cap/open-workspaces" }), /no dashboard/);
    assert.equal(hostConfig({ ...base, runner: undefined }).runner, undefined);
});


test("direct Tailscale hot-attaches only UDP 41641; existing host inputs are identical", async () => {
    created.length = 0;
    new Host("ow-direct", { ...hostArgs, subnetId: "subnet-private", associatePublicIpAddress: false });
    await settle();
    const before = resources();
    created.length = 0;
    new Host("ow-direct", { ...hostArgs, subnetId: "subnet-private", associatePublicIpAddress: false,
        tailscaleDirect: { subnetId: "subnet-public" } });
    await settle();
    const after = resources();
    for (const [key, inputs] of before) {
        if (key.includes("ssm/parameter:")) continue;
        assert.deepEqual(after.get(key), inputs, key);
    }
    const added = [...after.keys()].filter((key) => !before.has(key));
    assert.equal(added.length, 5, added.join(", "));
    const sg = byName("ow-direct-tailscale-direct", "securityGroup:").inputs;
    assert.deepEqual(sg.ingress, [{ protocol: "udp", fromPort: 41641, toPort: 41641, cidrBlocks: ["0.0.0.0/0"] }]);
    assert.deepEqual(sg.egress, [{ protocol: "udp", fromPort: 0, toPort: 65535, cidrBlocks: ["0.0.0.0/0"] }]);
    const eni = byName("ow-direct-tailscale-direct", "networkInterface:").inputs;
    assert.equal(eni.subnetId, "subnet-public");
    assert.deepEqual(eni.securityGroups, ["ow-direct-tailscale-direct-id"]);
    assert.deepEqual(byName("ow-direct-tailscale-direct", "networkInterfaceAttachment:").inputs,
        { instanceId: "ow-direct-host-id", networkInterfaceId: "ow-direct-tailscale-direct-id", deviceIndex: 1 });
    assert.equal(byName("ow-direct-tailscale-direct", "eipAssociation:").inputs.networkInterfaceId, "ow-direct-tailscale-direct-id");
    const config = JSON.parse(byName("ow-direct-config", "ssm/parameter:").inputs.insecureValue);
    assert.deepEqual(config.tailscale.direct, { mac: "02:00:00:00:00:01" });
});

test("direct Tailscale refuses another VPC, AZ or the primary subnet", () => {
    const host = { id: "private", vpcId: "vpc-1", availabilityZone: "us-east-1a" };
    assert.equal(tailscaleDirectSubnet(host, { ...host, id: "public" }), "public");
    for (const bad of [host, { ...host, id: "public", vpcId: "vpc-2" },
        { ...host, id: "public", availabilityZone: "us-east-1b" }]) {
        assert.throws(() => tailscaleDirectSubnet(host, bad), /separate public subnet/);
    }
});
