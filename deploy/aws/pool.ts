import * as fs from "fs";
import * as path from "path";
import * as aws from "@pulumi/aws";
import * as pulumi from "@pulumi/pulumi";

import { backupPolicy, hostConfig, renderUserData, storeRunnerPolicy } from "./index";
import type { ImageArtifact, RunnerController } from "./index";

export interface RunnerPoolArgs {
    /** The Host whose controller adds, removes and enrols this pool's runners. */
    controller: RunnerController;
    /** Pinned x86-64 Ubuntu 24.04 AMI. Required: a pool never follows a moving "current" image. */
    amiId: pulumi.Input<string>;
    /** c8i, m8i, r8i or flex: nested virtualization. */
    instanceType: pulumi.Input<string>;
    /** Subnets the group may launch in. The security group has no inbound rules either way. */
    subnetIds: pulumi.Input<pulumi.Input<string>[]>;
    /** Most instances the group may run (1 to 20). Retained instances are outside it: see docs/RUNNER_POOL.md. */
    maxSize: number;
    /** Operator price of one instance hour, in cents, for liability accounting. Never looked up. */
    instanceHourlyCents: number;
    /** Operator price of one GiB-month of EBS, in cents, for disk accounting. */
    diskMonthlyCentsPerGib: number;
    /** Seconds a removal waits for the controller before AWS abandons it and the instance is retained (30 to 7200). Default 3600. */
    drainTimeoutSeconds?: number;
    /** A public IPv4 address gives egress without a NAT gateway. Default true. */
    associatePublicIpAddress?: pulumi.Input<boolean>;
    /** Each instance's gp3 Btrfs data volume. It is never deleted with its instance. Default 100. */
    dataVolumeSizeGib?: pulumi.Input<number>;
    repository?: pulumi.Input<string>;
    /** Release tag or commit. Default: this package's release tag. */
    ref?: pulumi.Input<string>;
    tags?: pulumi.Input<{ [key: string]: pulumi.Input<string> }>;
    /** Full ARN of the Secrets Manager secret holding the Tailscale auth key: runners reach the controller over the tailnet. */
    tailscaleAuthKeySecretArn: pulumi.Input<string>;
    developerImages?: pulumi.Input<pulumi.Input<string>[]>;
    storeUrl?: pulumi.Input<string>;
    imageArtifacts?: pulumi.Input<{ [profile: string]: pulumi.Input<ImageArtifact> }>;
    /** Runner snapshots go to `backupUrl/runners/POOL`. */
    backupUrl?: pulumi.Input<string>;
    idleMinutes?: pulumi.Input<number>;
    vcpuOvercommit?: pulumi.Input<number>;
}

/** Stage 0 of bootstrap.sh, run unchanged: only the values ahead of it differ from a Host's. */
const STAGE0 = "2";
const DEFAULT_REPOSITORY = "https://github.com/sebassdc/open-workspaces.git";
/** The termination hook the controller completes for an exact, drained instance. */
export const DRAIN_HOOK = "open-workspaces-drain";
/** Processes that would let AWS launch or terminate on its own; the controller decides alone. */
export const SUSPENDED = ["AZRebalance", "AlarmNotification", "InstanceRefresh", "ReplaceUnhealthy", "ScheduledActions"];

/** A pool name prefixes each runner's node ID (NAME-VOLUME): 1 to 14 lowercase letters, digits or `-`. */
export function poolName(name: string): string {
    if (!/^[a-z][a-z0-9-]{0,13}$/.test(name)) {
        throw new Error(`a pool's name is its node prefix (1 to 14 lowercase letters, digits or -), not ${JSON.stringify(name)}`);
    }
    return name;
}

/** Checks the plain-number pool settings. */
export function poolLimits(args: Pick<RunnerPoolArgs, "maxSize" | "instanceHourlyCents" | "diskMonthlyCentsPerGib" | "drainTimeoutSeconds">) {
    const whole = (value: unknown, low: number, high: number) => Number.isInteger(value) && (value as number) >= low && (value as number) <= high;
    if (!whole(args.maxSize, 1, 20)) throw new Error("maxSize must be 1 to 20");
    if (!whole(args.instanceHourlyCents, 1, 100000) || !whole(args.diskMonthlyCentsPerGib, 1, 1000)) {
        throw new Error("instanceHourlyCents and diskMonthlyCentsPerGib must be whole positive cents");
    }
    const drain = args.drainTimeoutSeconds ?? 3600;
    if (!whole(drain, 30, 7200)) throw new Error("drainTimeoutSeconds must be 30 to 7200");
    return drain;
}

/**
 * Stage 0 for a pool instance: bootstrap.sh itself, after the volume the launch template
 * attached (the one EBS disk with nothing mounted) and the pool's shared config parameter.
 */
export function poolUserData(pool: string, region: string, script = fs.readFileSync(path.join(__dirname, "bootstrap.sh"), "utf8")): string {
    const discover = `# @OW_ENV@
OW_DATA_VOLUME_ID=
for _ in $(seq 900); do
  OW_DATA_VOLUME_ID=$(lsblk -dnpo NAME,SERIAL | while read -r dev serial; do
    if [[ $serial == vol* ]] && ! lsblk -no MOUNTPOINT "$dev" | grep -q .; then echo "vol-\${serial#vol}"; fi
  done | head -n1) || true
  [[ -n $OW_DATA_VOLUME_ID ]] && break
  sleep 1
done`;
    return renderUserData({
        OW_POOL: poolName(pool),
        OW_CONFIG_PARAMETER: `/open-workspaces/${pool}/config`,
        OW_REGION: region,
        OW_STAGE0: STAGE0,
    }, script.replace("# @OW_ENV@", () => discover));
}

/** The controller's rights over this pool: exact actions on its group, its join secrets, its disks. */
export function controllerPoolPolicy(partition: string, region: string, account: string, pool: string, groupArn: string, parameterArn: string,
    controllerRole: string): string {
    const secrets = `arn:${partition}:secretsmanager:${region}:${account}:secret:open-workspaces/pool/${poolName(pool)}/*`;
    return JSON.stringify({
        Version: "2012-10-17",
        Statement: [
            { Effect: "Allow", Resource: groupArn, Action: ["autoscaling:SetDesiredCapacity",
                "autoscaling:TerminateInstanceInAutoScalingGroup", "autoscaling:CompleteLifecycleAction"] },
            // Describe calls take no resource; they change nothing.
            { Effect: "Allow", Resource: "*", Action: ["autoscaling:DescribeAutoScalingGroups", "autoscaling:DescribeScalingActivities",
                "autoscaling:DescribeLifecycleHooks", "autoscaling:DescribePolicies", "ec2:DescribeInstances", "ec2:DescribeVolumes"] },
            { Effect: "Allow", Resource: secrets, Action: ["secretsmanager:CreateSecret", "secretsmanager:TagResource"],
                Condition: { StringEquals: { "aws:RequestTag/ow-pool": pool },
                    "ForAllValues:StringEquals": { "aws:TagKeys": ["ow-pool", "ow-session"] } } },
            { Effect: "Allow", Resource: secrets, Action: ["secretsmanager:GetSecretValue", "secretsmanager:PutSecretValue",
                "secretsmanager:DescribeSecret", "secretsmanager:DeleteSecret"] },
            { Effect: "Allow", Resource: parameterArn, Action: "ssm:GetParameter" },
            // `pool check`: a termination cannot be probed without a real instance, so the
            // controller asks IAM about its own role.
            { Effect: "Allow", Resource: `arn:${partition}:iam::${account}:role/${controllerRole}`, Action: "iam:SimulatePrincipalPolicy" },
            // Only a disk the controller has proved empty; see `ow-host pool delete-disk`.
            { Effect: "Allow", Resource: `arn:${partition}:ec2:${region}:${account}:volume/*`, Action: "ec2:DeleteVolume",
                Condition: { StringEquals: { "aws:ResourceTag/ow-pool": pool } } },
        ],
    });
}

/** A pool instance reads and answers only the join secret the controller tagged with its own session. */
export function runnerJoinPolicy(partition: string, region: string, account: string, pool: string): string {
    return JSON.stringify({
        Version: "2012-10-17",
        Statement: [{
            Effect: "Allow",
            Action: ["secretsmanager:GetSecretValue", "secretsmanager:PutSecretValue"],
            Resource: `arn:${partition}:secretsmanager:${region}:${account}:secret:open-workspaces/pool/${poolName(pool)}/*`,
            // aws:userid of an instance role session is ROLE-ID:INSTANCE-ID.
            Condition: { StringEquals: { "secretsmanager:ResourceTag/ow-session": "${aws:userid}" } },
        }],
    });
}

/**
 * An on-demand Auto Scaling group of runner hosts that the controller alone sizes:
 * desired capacity starts at 0 and is never managed by Pulumi, no scaling policy
 * exists, and AWS's own replacement processes are suspended. Removal is exact
 * (`ow-host pool remove`); a removal the controller does not complete in time is
 * abandoned and the instance retained. Data volumes outlive their instances.
 * Creating it starts no instance and changes no existing host.
 */
export class RunnerPool extends pulumi.ComponentResource {
    public readonly groupName: pulumi.Output<string>;
    public readonly groupArn: pulumi.Output<string>;
    public readonly launchTemplateId: pulumi.Output<string>;
    public readonly roleName: pulumi.Output<string>;
    public readonly securityGroupId: pulumi.Output<string>;
    /** The pool's shared config: `/open-workspaces/NAME/config`. */
    public readonly configParameter: pulumi.Output<string>;

    constructor(name: string, args: RunnerPoolArgs, opts?: pulumi.ComponentResourceOptions) {
        poolName(name);
        const drain = poolLimits(args);
        super("open-workspaces-aws:index:RunnerPool", name, {}, opts);
        const parent = { parent: this };
        const controller = args.controller;
        const tags = pulumi.output(args.tags ?? {}).apply((t) => ({ ...t, "ow-pool": name }));
        const partition = aws.getPartitionOutput({}, parent).partition;
        const region = aws.getRegionOutput({}, parent).region;
        const account = aws.getCallerIdentityOutput({}, parent).accountId;
        const groupName = `open-workspaces-${name}`;

        const role = new aws.iam.Role(`${name}-pool`, {
            assumeRolePolicy: JSON.stringify({ Version: "2012-10-17",
                Statement: [{ Effect: "Allow", Principal: { Service: "ec2.amazonaws.com" }, Action: "sts:AssumeRole" }] }),
            tags,
        }, parent);
        new aws.iam.RolePolicyAttachment(`${name}-ssm`, {
            role: role.name, policyArn: pulumi.interpolate`arn:${partition}:iam::aws:policy/AmazonSSMManagedInstanceCore`,
        }, parent);
        new aws.iam.RolePolicy(`${name}-tailscale-key`, { role: role.id, policy: pulumi.output(args.tailscaleAuthKeySecretArn).apply((arn) =>
            JSON.stringify({ Version: "2012-10-17", Statement: [{ Effect: "Allow", Action: "secretsmanager:GetSecretValue", Resource: arn }] })) }, parent);
        new aws.iam.RolePolicy(`${name}-join`, { role: role.id,
            policy: pulumi.all([partition, region, account]).apply(([p, r, a]) => runnerJoinPolicy(p, r, a, name)) }, parent);
        if (args.storeUrl) {
            new aws.iam.RolePolicy(`${name}-store`, { role: role.id,
                policy: pulumi.all([partition, args.storeUrl]).apply(([p, url]) => storeRunnerPolicy(p, url)) }, parent);
        }
        const backupUrl = args.backupUrl && pulumi.output(args.backupUrl).apply((url) => `${url.replace(/\/+$/, "")}/runners/${name}`);
        if (backupUrl) {
            new aws.iam.RolePolicy(`${name}-backup`, { role: role.id,
                policy: pulumi.all([partition, backupUrl]).apply(([p, url]) => backupPolicy(p, url, false)) }, parent);
        }

        const config = new aws.ssm.Parameter(`${name}-config`, {
            name: `/open-workspaces/${name}/config`,
            type: "String",
            tier: "Standard",
            insecureValue: pulumi.all([
                pulumi.all([args.repository ?? DEFAULT_REPOSITORY, args.ref ?? `v${require("./package.json").version}`, args.tailscaleAuthKeySecretArn]),
                pulumi.all([args.developerImages ?? [], backupUrl ?? "", args.idleMinutes ?? 30, args.vcpuOvercommit ?? 1]),
                pulumi.all([args.storeUrl ?? "", args.imageArtifacts ?? {}]),
                pulumi.all([partition, region, account, role.uniqueId]),
            ]).apply(([[repository, ref, secretArn], [images, backup, idleMinutes, vcpuOvercommit], [storeUrl, imageArtifacts], [p, r, a, roleId]]) =>
                JSON.stringify(hostConfig({
                    repository, ref, tailscaleAuthKeySecretArn: secretArn, tailscaleHostname: "", tailscaleSsh: false,
                    dashboardLogins: [], developerImages: images, backupUrl: backup, backupSchedule: "", idleMinutes, vcpuOvercommit,
                    storeUrl, imageArtifacts,
                    pool: { name, controller: controller.hostName, group: groupName, roleId,
                        joinSecretPrefix: `arn:${p}:secretsmanager:${r}:${a}:secret:open-workspaces/pool/${name}/`,
                        hook: DRAIN_HOOK, instanceHourlyCents: args.instanceHourlyCents, diskMonthlyCentsPerGib: args.diskMonthlyCentsPerGib },
                }))),
            tags,
        }, parent);
        new aws.iam.RolePolicy(`${name}-config`, { role: role.id, policy: config.arn.apply((arn) =>
            JSON.stringify({ Version: "2012-10-17", Statement: [{ Effect: "Allow", Action: "ssm:GetParameter", Resource: arn }] })) }, parent);
        const profile = new aws.iam.InstanceProfile(`${name}-pool`, { role: role.name, tags }, parent);

        const firstSubnet = aws.ec2.getSubnetOutput({ id: pulumi.output(args.subnetIds).apply((ids) => {
            if (ids.length === 0) throw new Error("subnetIds needs at least one subnet");
            return ids[0];
        }) }, parent);
        // No ingress rules: SSM and Tailscale both connect outbound.
        const securityGroup = new aws.ec2.SecurityGroup(`${name}-pool`, {
            description: "Open Workspaces runner pool: no inbound access",
            vpcId: firstSubnet.vpcId,
            egress: [{ protocol: "-1", fromPort: 0, toPort: 0, cidrBlocks: ["0.0.0.0/0"], ipv6CidrBlocks: ["::/0"] }],
            tags,
        }, parent);

        const ami = aws.ec2.getAmiOutput({ filters: [{ name: "image-id", values: [args.amiId] }] }, parent);
        const template = new aws.ec2.LaunchTemplate(`${name}-pool`, {
            imageId: args.amiId,
            instanceType: args.instanceType,
            iamInstanceProfile: { arn: profile.arn },
            networkInterfaces: [{ deviceIndex: 0, associatePublicIpAddress: pulumi.output(args.associatePublicIpAddress ?? true).apply(String),
                securityGroups: [securityGroup.id], deleteOnTermination: "true" }],
            cpuOptions: { nestedVirtualization: "enabled" },
            metadataOptions: { httpEndpoint: "enabled", httpTokens: "required", httpPutResponseHopLimit: 1 },
            blockDeviceMappings: [
                { deviceName: ami.rootDeviceName, ebs: { volumeType: "gp3", volumeSize: 30, encrypted: "true", deleteOnTermination: "true" } },
                // The dev data: retained when its instance goes, for salvage through a replacement.
                { deviceName: "/dev/sdf", ebs: { volumeType: "gp3", volumeSize: args.dataVolumeSizeGib ?? 100, encrypted: "true", deleteOnTermination: "false" } },
            ],
            userData: region.apply((r) => Buffer.from(poolUserData(name, r)).toString("base64")),
            tagSpecifications: ["instance", "volume"].map((resourceType) => ({ resourceType, tags: tags.apply((t) => ({ ...t, Name: name })) })),
            updateDefaultVersion: true,
            tags,
        }, parent);

        const group = new aws.autoscaling.Group(`${name}-pool`, {
            name: groupName,
            minSize: 0,
            maxSize: args.maxSize,
            desiredCapacity: 0,
            vpcZoneIdentifiers: args.subnetIds,
            launchTemplate: { id: template.id, version: template.latestVersion.apply(String) },
            healthCheckType: "EC2",
            protectFromScaleIn: true,
            suspendedProcesses: SUSPENDED,
            initialLifecycleHooks: [{ name: DRAIN_HOOK, lifecycleTransition: "autoscaling:EC2_INSTANCE_TERMINATING",
                defaultResult: "ABANDON", heartbeatTimeout: drain }],
            // ABANDON alone terminates; this policy retains the instance instead.
            instanceLifecyclePolicy: { retentionTriggers: { terminateHookAbandon: "retain" } },
            waitForCapacityTimeout: "0",
            tags: [{ key: "ow-pool", value: name, propagateAtLaunch: true }],
        }, { parent: this, ignoreChanges: ["desiredCapacity"] });

        new aws.iam.RolePolicy(`${name}-controller`, {
            role: controller.roleName,
            policy: pulumi.all([partition, region, account, group.arn, config.arn, controller.roleName])
                .apply(([p, r, a, groupArn, parameterArn, roleName]) => controllerPoolPolicy(p, r, a, name, groupArn, parameterArn, roleName)),
        }, parent);

        this.groupName = group.name;
        this.groupArn = group.arn;
        this.launchTemplateId = template.id;
        this.roleName = role.name;
        this.securityGroupId = securityGroup.id;
        this.configParameter = config.name;
        this.registerOutputs({ groupName: this.groupName, groupArn: this.groupArn, launchTemplateId: this.launchTemplateId,
            roleName: this.roleName, securityGroupId: this.securityGroupId, configParameter: this.configParameter });
    }
}
