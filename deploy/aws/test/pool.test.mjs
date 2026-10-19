import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import * as pulumi from "@pulumi/pulumi";

// As runner.test.mjs: CommonJS globals, then mocks before the component loads.
globalThis.__dirname = fileURLToPath(new URL("..", import.meta.url));
globalThis.require = createRequire(new URL("../index.ts", import.meta.url));

const created = [];
const calls = [];
pulumi.runtime.setMocks({
    newResource: (args) => {
        created.push(args);
        return { id: `${args.name}-id`, state: { name: args.name, ...args.inputs, arn: `arn:aws:mock:::${args.name}`,
            uniqueId: "AROAPOOLROLE", latestVersion: 3 } };
    },
    call: (args) => {
        calls.push(args.token);
        return ({
            "aws:ec2/getVpc:getVpc": { id: "vpc-1" },
            "aws:ec2/getInstanceTypeOfferings:getInstanceTypeOfferings": { locations: ["us-east-1a"] },
            "aws:ec2/getSubnets:getSubnets": { ids: ["subnet-1"] },
            "aws:ec2/getSubnet:getSubnet": { id: args.inputs.id ?? "subnet-1", vpcId: "vpc-1", availabilityZone: "us-east-1a" },
            "aws:index/getPartition:getPartition": { partition: "aws" },
            "aws:index/getRegion:getRegion": { region: "us-east-1", name: "us-east-1" },
            "aws:index/getCallerIdentity:getCallerIdentity": { accountId: "111122223333" },
            "aws:ec2/getAmi:getAmi": { rootDeviceName: "/dev/sda1" },
            "aws:ssm/getParameter:getParameter": { value: "ami-current" },
        })[args.token] ?? {};
    },
}, "project", "stack", false);
const { Host, Runner, RunnerPool, poolName, poolLimits, poolUserData, controllerPoolPolicy, renderUserData } = await import("../index.ts");

const settle = () => new Promise((resolve) => setTimeout(resolve, 200));
const secretArn = "arn:aws:secretsmanager:us-east-1:111122223333:secret:ow/ts-AbCdEf";
const hostArgs = { tailscaleAuthKeySecretArn: secretArn, backupUrl: "s3://my-bucket/ow" };
const poolArgs = (host) => ({ controller: { hostName: host.hostName, roleName: host.roleName }, amiId: "ami-pinned",
    instanceType: "m8i-flex.xlarge", subnetIds: ["subnet-a", "subnet-b"], maxSize: 3, instanceHourlyCents: 21,
    diskMonthlyCentsPerGib: 8, tailscaleAuthKeySecretArn: secretArn, backupUrl: "s3://my-bucket/ow", ref: "v9.9.9" });
const byName = (name, type = "") => created.find((r) => r.name === name && r.type.includes(type));
const policy = (name) => JSON.parse(byName(name, "rolePolicy:").inputs.policy).Statement;
const resources = () => new Map(created.map((r) => [`${r.type} ${r.name}`, r.inputs]));

async function stack(withPool) {
    created.length = 0;
    calls.length = 0;
    const host = new Host("ow-ctl", hostArgs);
    new Runner("ow-big", { controller: host, tailscaleAuthKeySecretArn: secretArn });
    if (withPool) new RunnerPool("dev", poolArgs(host));
    await settle();
    return resources();
}

test("adding a pool starts no instance and changes no existing host or runner", async () => {
    const before = await stack(false);
    const after = await stack(true);
    for (const [key, inputs] of before) assert.deepEqual(after.get(key), inputs, key);
    const added = [...after.keys()].filter((key) => !before.has(key));
    assert.ok(added.every((key) => / dev(-|$)/.test(key)), added.join(", "));
    assert.ok(!added.some((key) => key.includes("ec2/instance:") || key.includes("ebs/volume:")), "no instance or volume is created");
    const group = byName("dev-pool", "autoscaling/group:").inputs;
    assert.deepEqual([group.minSize, group.desiredCapacity, group.maxSize], [0, 0, 3]);
    assert.ok(!created.some((r) => /autoscaling\/(policy|schedule)/.test(r.type)), "no AWS scaling policy");
    // The one resource that names the controller: its role gets this pool's rights.
    const touching = added.filter((key) => JSON.stringify(after.get(key)).includes("ow-ctl-host"));
    assert.deepEqual(touching, ["aws:iam/rolePolicy:RolePolicy dev-controller"]);
});

test("the launch template pins the AMI, release and stage 0, with no inbound access", async () => {
    await stack(false);
    const lookups = calls.filter((c) => c === "aws:ssm/getParameter:getParameter").length;
    await stack(true);
    assert.equal(calls.filter((c) => c === "aws:ssm/getParameter:getParameter").length, lookups, "no moving AMI lookup for the pool");
    const lt = byName("dev-pool", "launchTemplate:").inputs;
    assert.equal(lt.imageId, "ami-pinned");
    assert.deepEqual(lt.cpuOptions, { nestedVirtualization: "enabled" });
    assert.deepEqual(lt.metadataOptions, { httpEndpoint: "enabled", httpTokens: "required", httpPutResponseHopLimit: 1 });
    assert.deepEqual(lt.networkInterfaces[0].securityGroups, ["dev-pool-id"]);
    assert.equal(byName("dev-pool", "securityGroup:").inputs.ingress, undefined, "no inbound rules");
    const [root, data] = lt.blockDeviceMappings;
    assert.deepEqual([root.deviceName, root.ebs.deleteOnTermination, root.ebs.encrypted], ["/dev/sda1", "true", "true"]);
    assert.deepEqual([data.deviceName, data.ebs.deleteOnTermination, data.ebs.encrypted, data.ebs.volumeSize], ["/dev/sdf", "false", "true", 100]);
    const config = JSON.parse(byName("dev-config", "ssm/parameter:").inputs.insecureValue);
    assert.equal(config.role, "runner");
    assert.deepEqual(config.release, { repository: "https://github.com/sebassdc/open-workspaces.git", ref: "v9.9.9" });
    assert.deepEqual(config.pool, { name: "dev", controller: "ow-ctl", group: "open-workspaces-dev", roleId: "AROAPOOLROLE",
        joinSecretPrefix: "arn:aws:secretsmanager:us-east-1:111122223333:secret:open-workspaces/pool/dev/",
        hook: "open-workspaces-drain", instanceHourlyCents: 21, diskMonthlyCentsPerGib: 8 });
    assert.equal(config.backup.url, "s3://my-bucket/ow/runners/dev");
    assert.ok(!("runner" in config), "each instance derives its node from its data volume");
    // Stage 0 is a Host's bootstrap.sh, byte for byte, after the pool's own values.
    const userData = Buffer.from(lt.userData, "base64").toString();
    const hostData = byName("ow-ctl-host", "ec2/instance:").inputs.userData;
    const body = (s) => s.slice(s.indexOf("\nstatus=/var/lib/open-workspaces/bootstrap-status"));
    assert.equal(body(userData), body(hostData));
    assert.match(userData, /^OW_POOL='dev'$/m);
    assert.match(userData, /^OW_CONFIG_PARAMETER='\/open-workspaces\/dev\/config'$/m);
    assert.ok(userData.indexOf("OW_DATA_VOLUME_ID=$(") < userData.indexOf("nvme-Amazon_Elastic_Block_Store_${OW_DATA_VOLUME_ID"));
});

test("only the controller sizes the group; an abandoned removal retains the instance", async () => {
    await stack(true);
    const group = byName("dev-pool", "autoscaling/group:").inputs;
    assert.deepEqual(group.initialLifecycleHooks, [{ name: "open-workspaces-drain", lifecycleTransition: "autoscaling:EC2_INSTANCE_TERMINATING",
        defaultResult: "ABANDON", heartbeatTimeout: 3600 }]);
    assert.deepEqual(group.instanceLifecyclePolicy, { retentionTriggers: { terminateHookAbandon: "retain" } });
    assert.deepEqual(group.suspendedProcesses, ["AZRebalance", "AlarmNotification", "InstanceRefresh", "ReplaceUnhealthy", "ScheduledActions"]);
    assert.equal(group.protectFromScaleIn, true, "no instance goes by a desired-capacity decrease");
    assert.deepEqual(group.vpcZoneIdentifiers, ["subnet-a", "subnet-b"]);
    assert.deepEqual(group.launchTemplate, { id: "dev-pool-id", version: "3" });
});

test("rights: exact group actions, tagged secrets and disks, no real secret in Pulumi", async () => {
    await stack(true);
    assert.ok(!created.some((r) => r.name.startsWith("dev") && r.type.includes("secretsmanager/")), "no secret held by Pulumi");
    const ctl = policy("dev-controller");
    assert.equal(byName("dev-controller", "rolePolicy:").inputs.role, "ow-ctl-host");
    assert.deepEqual(ctl[0], { Effect: "Allow", Resource: "arn:aws:mock:::dev-pool",
        Action: ["autoscaling:SetDesiredCapacity", "autoscaling:TerminateInstanceInAutoScalingGroup", "autoscaling:CompleteLifecycleAction"] });
    assert.ok(ctl[1].Action.every((a) => /:Describe/.test(a)), "wildcard resources only for describe calls");
    const prefix = "arn:aws:secretsmanager:us-east-1:111122223333:secret:open-workspaces/pool/dev/*";
    assert.deepEqual([ctl[2].Resource, ctl[3].Resource], [prefix, prefix]);
    assert.deepEqual(ctl[2].Condition.StringEquals, { "aws:RequestTag/ow-pool": "dev" });
    assert.deepEqual(ctl[5].Condition, { StringEquals: { "aws:ResourceTag/ow-pool": "dev" } });
    assert.equal(ctl[5].Action, "ec2:DeleteVolume");
    assert.deepEqual(policy("dev-join"), [{ Effect: "Allow", Action: ["secretsmanager:GetSecretValue", "secretsmanager:PutSecretValue"],
        Resource: prefix, Condition: { StringEquals: { "secretsmanager:ResourceTag/ow-session": "${aws:userid}" } } }]);
    assert.deepEqual(policy("dev-backup")[1].Resource, ["arn:aws:s3:::my-bucket/ow/runners/dev/catalog/*", "arn:aws:s3:::my-bucket/ow/runners/dev/snapshots/*"]);
    // The same rendering the component uses, from plain values.
    assert.deepEqual(JSON.parse(controllerPoolPolicy("aws", "us-east-1", "111122223333", "dev", "arn:aws:mock:::dev-pool", "arn:aws:mock:::dev-config")).Statement, ctl);
});

test("pool names and limits are checked before anything is created", () => {
    for (const bad of ["", "Dev", "1dev", "x".repeat(15), "has_underscore"]) assert.throws(() => poolName(bad), /node prefix/, bad);
    const ok = { maxSize: 2, instanceHourlyCents: 21, diskMonthlyCentsPerGib: 8 };
    assert.equal(poolLimits(ok), 3600);
    for (const bad of [{ maxSize: 0 }, { maxSize: 21 }, { maxSize: 1.5 }, { instanceHourlyCents: 0 }, { diskMonthlyCentsPerGib: -1 },
        { drainTimeoutSeconds: 29 }, { drainTimeoutSeconds: 7201 }]) {
        assert.throws(() => poolLimits({ ...ok, ...bad }), /must be/, JSON.stringify(bad));
    }
    assert.throws(() => new RunnerPool("Bad", {}), /node prefix/);
    assert.throws(() => poolUserData("dev'x", "us-east-1"), /node prefix/);
    assert.match(poolUserData("dev", "us-east-1", "a\n# @OW_ENV@\nb"), /^OW_POOL='dev'\nOW_CONFIG_PARAMETER=.*\nOW_REGION='us-east-1'\nOW_STAGE0='2'\nOW_DATA_VOLUME_ID=$/m);
    assert.equal(typeof renderUserData, "function");
});
