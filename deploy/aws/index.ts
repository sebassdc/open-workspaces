import * as fs from "fs";
import * as path from "path";
import * as aws from "@pulumi/aws";
import * as pulumi from "@pulumi/pulumi";

import { storeRunnerPolicy, storeGcPolicy } from "./store";
export { ArtifactStore, storeLifecycleRules, tlsOnlyPolicy, storeBucketPolicy, storeRunnerPolicy, storeGcPolicy } from "./store";
export type { ArtifactStoreArgs } from "./store";

export interface ImageArtifact {
    version: pulumi.Input<number>;
    attempt: pulumi.Input<string>;
    digest: pulumi.Input<string>;
}

export interface TailscaleDirectArgs {
    /** Public subnet in the host's VPC and availability zone, routed to an internet gateway. */
    subnetId: pulumi.Input<string>;
}

export interface HostArgs {
    /** host (default): controller plus local runner; controller: remote capacity only, no KVM. New hosts only. */
    role?: pulumi.Input<string>;
    /** Combined host/runner: c8i, m8i, r8i or flex with nested virtualization; controller: any suitable x86-64 type. */
    instanceType?: pulumi.Input<string>;
    /** Subnet for the host. Default: the default VPC's default subnet in the first zone offering the type. */
    subnetId?: pulumi.Input<string>;
    /** A public IPv4 address gives egress without a NAT gateway. The security group still has no inbound rules. */
    associatePublicIpAddress?: pulumi.Input<boolean>;
    /** x86-64 Ubuntu 24.04 AMI. Default: Canonical's current image from SSM. A new image replaces the host, never the data volume. */
    amiId?: pulumi.Input<string>;
    /** Size of the gp3 Btrfs volume holding sources, assets and workspaces. */
    dataVolumeSizeGib?: pulumi.Input<number>;
    /** Provisioned IOPS of the data volume (gp3: 3000-16000). Default: gp3's 3000. Changes apply online. */
    dataVolumeIops?: pulumi.Input<number>;
    /** Provisioned throughput of the data volume in MiB/s (gp3: 125-1000). Default: gp3's 125. Changes apply online. */
    dataVolumeThroughput?: pulumi.Input<number>;
    /** Git repository with the Open Workspaces sources. */
    repository?: pulumi.Input<string>;
    /**
     * Git tag or commit to build on the host. Default: this package's release tag. Part of
     * the host's config parameter: a change is applied in place by `ow-host upgrade`.
     */
    ref?: pulumi.Input<string>;
    /** Tags for every resource the component creates. */
    tags?: pulumi.Input<{ [key: string]: pulumi.Input<string> }>;
    /** Full ARN of a Secrets Manager secret holding a Tailscale auth key. Omit to skip Tailscale. */
    tailscaleAuthKeySecretArn?: pulumi.Input<string>;
    /** Tailnet hostname. Default: the component name. */
    tailscaleHostname?: pulumi.Input<string>;
    /** Enable Tailscale SSH, authorized by the tailnet policy. */
    tailscaleSsh?: pulumi.Input<boolean>;
    /** Attach a secondary interface and Elastic IP for direct Tailscale UDP 41641 only; never moves or stops the host. */
    tailscaleDirect?: pulumi.Input<TailscaleDirectArgs>;
    /**
     * Tailnet logins (for example `you@example.com`) allowed into the web dashboard.
     * The dashboard listens on loopback and is served to the tailnet only, through
     * `tailscale serve`. Needs `tailscaleAuthKeySecretArn`. Alternative to dashboardCapability.
     */
    dashboardLogins?: pulumi.Input<pulumi.Input<string>[]>;
    /** Application capability (domain/path) admitting tailnet users. Needs tailscaleAuthKeySecretArn.
     * Omit both this and dashboardLogins for no dashboard. When both are set, either admits a user. */
    dashboardCapability?: pulumi.Input<string>;
    /**
     * Developer images (`ubuntu` only for now) to build on the host after boot: the `dev`
     * user, SSH server and toolchains that SSH access and golden-image recipes need.
     * Built once per data volume, in the background. Default: none (Alpine only).
     */
    developerImages?: pulumi.Input<pulumi.Input<string>[]>;
    /**
     * Backups to S3, as `s3://bucket/prefix`. The host role may list the bucket under the
     * prefix and read and write objects there (no delete: retention is the bucket's), and
     * the host gets the AWS CLI and `/usr/local/bin/ow-backup`: it copies the controller
     * catalog to `PREFIX/catalog/` and uploads every sealed snapshot the store lacks
     * (hibernated boxes' captures included) to `PREFIX/snapshots/`. Run it from your own
     * scheduler, best right after `ow down --hibernate`, when every running box is sealed,
     * or set `backupSchedule`. Default: no backups.
     */
    backupUrl?: pulumi.Input<string>;
    /** Opt-in write-once image store root, s3://bucket/prefix. No list/delete rights. */
    storeUrl?: pulumi.Input<string>;
    /** Controller-only GC rights. Default false; never enables a timer or deletion. Needs storeUrl. */
    storeGc?: boolean;
    /** Digest-pinned base images to pull instead of building developerImages locally. */
    imageArtifacts?: pulumi.Input<{ [profile: string]: pulumi.Input<ImageArtifact> }>;
    /**
     * When `ow-backup` runs, as a systemd calendar expression (for example
     * `*-*-* 01:00:00 UTC`); the host installs a timer for it. Needs `backupUrl`.
     * Default: no timer.
     */
    backupSchedule?: pulumi.Input<string>;
    /** Opt in to 30-minute running recovery points and upload after 10 hibernated
     * minutes. Requires backupUrl, a compatible upgraded worker and safe bucket
     * lifecycle (no current snapshot/chunk expiry). Default: false. */
    recoveryPoints?: pulumi.Input<boolean>;
    /** Minutes without a connection or activity before a box hibernates; 0 turns it off. Default: 30. */
    idleMinutes?: pulumi.Input<number>;
    /** CPU admission ratio: 1 (default) or 2 for dev workloads; RAM, slots and disk are unchanged. */
    vcpuOvercommit?: pulumi.Input<number>;
}

/** A runner host: Host inputs except dashboard, role and GC rights, plus the controller it joins. */
export interface RunnerArgs extends Omit<HostArgs, "dashboardLogins" | "dashboardCapability" | "role" | "storeGc"> {
    // The package analyzer loses optional types inherited through Omit.
    vcpuOvercommit?: pulumi.Input<number>;
    dataVolumeSizeGib?: pulumi.Input<number>;
    dataVolumeIops?: pulumi.Input<number>;
    dataVolumeThroughput?: pulumi.Input<number>;
    tailscaleDirect?: pulumi.Input<TailscaleDirectArgs>;
    storeUrl?: pulumi.Input<string>;
    imageArtifacts?: pulumi.Input<{ [profile: string]: pulumi.Input<ImageArtifact> }>;
    /**
     * The Host whose controller this runner joins (a Host itself fits): its component
     * name and role. That role may write this runner's join secret; nothing of that host
     * changes otherwise.
     */
    controller: RunnerController;
}

/** A plain object, not the Host: a component cannot be an input of a Pulumi package. */
export interface RunnerController {
    /** The Host's component name. */
    hostName: string;
    /** The Host's `roleName` output. */
    roleName: pulumi.Input<string>;
}

/** Host layout of stage 0 (bootstrap.sh); bump it with any change to that script. */
const STAGE0 = "2";
const DEFAULT_REPOSITORY = "https://github.com/sebassdc/open-workspaces.git";
const UBUNTU_AMI_PARAMETER = "/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id";

/** Values become `KEY='value'` lines ahead of bootstrap.sh; single quotes are escaped. */
export function renderUserData(
    env: Record<string, string>,
    script = fs.readFileSync(path.join(__dirname, "bootstrap.sh"), "utf8"),
): string {
    const lines = Object.entries(env).map(([key, value]) => `${key}='${value.replace(/'/g, `'\\''`)}'`);
    // A replacer function keeps `$&`, `$$` and similar in values literal.
    return script.replace("# @OW_ENV@", () => lines.join("\n"));
}

/** Checks dashboard logins and joins them for the user data. */
export function dashboardLoginList(logins: string[]): string {
    for (const login of logins) {
        if (!/^[A-Za-z0-9._%+@-]+@[A-Za-z0-9.-]+$/.test(login) || login.length > 254) {
            throw new Error(`invalid dashboard login: ${JSON.stringify(login)}`);
        }
    }
    return logins.join(",");
}

/** Checks developer image names and joins them for the user data. */
export function developerImageList(images: string[]): string {
    for (const image of images) {
        // Arch's bootstrap archive is .tar.zst, which the host's Python 3.12 tarfile cannot read.
        if (image !== "ubuntu") {
            throw new Error(`developerImages takes ubuntu, not ${JSON.stringify(image)}`);
        }
    }
    return [...new Set(images)].join(" ");
}

/** Splits a backup URL into bucket and prefix; refuses anything but `s3://bucket/prefix`. */
export function backupLocation(url: string): { bucket: string; prefix: string } {
    const match = /^s3:\/\/([a-z0-9][a-z0-9.-]{1,61}[a-z0-9])\/([A-Za-z0-9_-][A-Za-z0-9._-]*(?:\/[A-Za-z0-9_-][A-Za-z0-9._-]*)*)\/?$/.exec(url);
    if (!match || match[2].split("/").some((part) => part === "." || part === "..")) {
        throw new Error(`backupUrl must be s3://bucket/prefix, not ${JSON.stringify(url)}`);
    }
    return { bucket: match[1], prefix: match[2] };
}

/** What a runner's config adds: its node ID, its controller's name and its join secret. */
export interface RunnerConfig {
    node: string;
    controller: string;
    joinSecretArn: string;
}

export interface HostConfigInputs {
    role?: string;
    /** Set for a runner host (role runner). */
    runner?: RunnerConfig;
    repository: string;
    ref: string;
    tailscaleAuthKeySecretArn: string;
    tailscaleHostname: string;
    tailscaleSsh: boolean;
    dashboardLogins: string[];
    dashboardCapability?: string;
    developerImages: string[];
    backupUrl: string;
    storeUrl?: string;
    imageArtifacts?: { [profile: string]: { version: number; attempt: string; digest: string } };
    backupSchedule: string;
    recoveryPoints?: boolean;
    idleMinutes: number;
    vcpuOvercommit?: number;
    directMac?: string;
}

/**
 * The host's config parameter (`/open-workspaces/NAME/config`): everything the host
 * applies in place. The host validates it again; see deploy/aws/host/ow_host.py.
 */
export function hostConfig(c: HostConfigInputs): object {
    const role = c.role ?? "host";
    if (!["host", "controller"].includes(role) || (c.runner && role !== "host")) {
        throw new Error("role must be host or controller; Runner determines its own role");
    }
    if (role === "controller" && (c.developerImages.length || c.recoveryPoints || Object.keys(c.imageArtifacts ?? {}).length)) {
        throw new Error("controller role has no local images or recovery worker; configure images/recovery on runners");
    }
    const capability = c.dashboardCapability ?? "";
    if (capability) {
        const [domain, ...parts] = capability.split("/");
        if (capability.length > 512 || domain.length > 253 || !domain.includes(".")
            || !domain.split(".").every((part) => /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/.test(part))
            || !/^[A-Za-z0-9._/-]+$/.test(parts.join("/"))) {
            throw new Error("dashboardCapability must be domain/path (at most 512 characters)");
        }
    }
    if ((c.dashboardLogins.length > 0 || capability) && !c.tailscaleAuthKeySecretArn) {
        throw new Error("dashboardLogins/dashboardCapability needs tailscaleAuthKeySecretArn: the dashboard is served over the tailnet only");
    }
    if (c.directMac && (!c.tailscaleAuthKeySecretArn || !/^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$/.test(c.directMac))) {
        throw new Error("tailscaleDirect needs tailscaleAuthKeySecretArn and a valid interface MAC");
    }
    dashboardLoginList(c.dashboardLogins);
    if (c.storeUrl) storeRunnerPolicy("aws", c.storeUrl);
    const artifacts = c.imageArtifacts ?? {};
    if (Object.keys(artifacts).length && !c.storeUrl) throw new Error("imageArtifacts needs storeUrl");
    for (const [profile, a] of Object.entries(artifacts)) {
        if (!["ubuntu", "arch", "alpine"].includes(profile) || !Number.isSafeInteger(a.version) || a.version < 1
            || !/^[0-9a-f]{32}$/.test(a.attempt) || !/^[0-9a-f]{64}$/.test(a.digest)) throw new Error("invalid imageArtifacts pin");
    }
    const backup = c.backupUrl ? backupLocation(c.backupUrl) : undefined;
    if (c.backupSchedule && (!backup || !/^[A-Za-z0-9 *:/,.~_-]{1,100}$/.test(c.backupSchedule))) {
        throw new Error(`backupSchedule needs backupUrl and a systemd calendar expression, not ${JSON.stringify(c.backupSchedule)}`);
    }
    const vcpuOvercommit = c.vcpuOvercommit === undefined ? 1 : c.vcpuOvercommit;
    if (vcpuOvercommit !== 1 && vcpuOvercommit !== 2) {
        throw new Error("vcpuOvercommit must be 1 or 2");
    }
    if (c.recoveryPoints && !backup) throw new Error("recoveryPoints needs backupUrl");
    if (!Number.isInteger(c.idleMinutes) || c.idleMinutes < 0 || c.idleMinutes > 10080) {
        throw new Error(`idleMinutes must be a whole number from 0 to 10080, not ${c.idleMinutes}`);
    }
    if (!/^[A-Za-z0-9][A-Za-z0-9._/-]{0,127}$/.test(c.ref) || c.ref.includes("..")) {
        throw new Error(`ref must be a tag, branch or commit, not ${JSON.stringify(c.ref)}`);
    }
    if (!/^(https:\/\/[A-Za-z0-9._~:/@%+-]+|\/[A-Za-z0-9._/-]+)$/.test(c.repository)) {
        throw new Error(`repository must be an https URL, not ${JSON.stringify(c.repository)}`);
    }
    const images = developerImageList(c.developerImages);
    if (c.runner) {
        nodeId(c.runner.node);
        if (!c.tailscaleAuthKeySecretArn) {
            throw new Error("a runner needs tailscaleAuthKeySecretArn: its controller listens on the tailnet only");
        }
        if (c.dashboardLogins.length > 0 || capability) throw new Error("a runner has no dashboard");
    }
    return {
        schema: 1,
        role: c.runner ? "runner" : role,
        release: { repository: c.repository, ref: c.ref },
        tailscale: { secretArn: c.tailscaleAuthKeySecretArn, hostname: c.tailscaleHostname, ssh: c.tailscaleSsh,
            ...(c.directMac ? { direct: { mac: c.directMac } } : {}) },
        dashboard: { logins: c.dashboardLogins, ...(capability ? { capability } : {}) },
        developerImages: images ? images.split(" ") : [],
        ...(c.storeUrl ? { store: { url: c.storeUrl, images: artifacts } } : {}),
        backup: { url: backup ? `s3://${backup.bucket}/${backup.prefix}` : "", schedule: c.backupSchedule, ...(c.recoveryPoints ? { recovery: true } : {}) },
        idleMinutes: c.idleMinutes,
        vcpuOvercommit,
        ...(c.runner ? { runner: c.runner } : {}),
    };
}

/** A hot-attached interface must stay in this VPC/AZ, on its own public subnet. */
export function tailscaleDirectSubnet(
    host: Pick<aws.ec2.GetSubnetResult, "id" | "vpcId" | "availabilityZone">,
    direct: Pick<aws.ec2.GetSubnetResult, "id" | "vpcId" | "availabilityZone">,
): string {
    if (host.vpcId !== direct.vpcId || host.availabilityZone !== direct.availabilityZone || host.id === direct.id) {
        throw new Error("tailscaleDirect needs a separate public subnet in the host's VPC and availability zone");
    }
    return direct.id;
}

/** A runner's node ID is its component name: 1 to 32 letters, digits, `-` or `_`. */
export function nodeId(name: string): string {
    if (!/^[A-Za-z0-9_-]{1,32}$/.test(name) || name === "local") {
        throw new Error(`a runner's name is its node ID (1 to 32 letters, digits, - or _), not ${JSON.stringify(name)}`);
    }
    return name;
}

/** Own backup writes; controllers also read runner snapshots. Never grants store access or deletion. */
export function backupPolicy(partition: string, url: string, includeRunnerSnapshots = true): string {
    const { bucket, prefix } = backupLocation(url);
    const runnerSnapshots = `${prefix}/runners/*/snapshots/*`;
    return JSON.stringify({
        Version: "2012-10-17",
        Statement: [
            {
                Effect: "Allow",
                Action: "s3:ListBucket",
                Resource: `arn:${partition}:s3:::${bucket}`,
                Condition: { StringLike: { "s3:prefix": [`${prefix}/catalog/*`, `${prefix}/snapshots/*`,
                    ...(includeRunnerSnapshots ? [runnerSnapshots] : [])] } },
            },
            {
                Effect: "Allow",
                Action: ["s3:GetObject", "s3:PutObject", "s3:AbortMultipartUpload"],
                Resource: ["catalog", "snapshots"].map((p) => `arn:${partition}:s3:::${bucket}/${prefix}/${p}/*`),
            },
            ...(includeRunnerSnapshots ? [{ Effect: "Allow", Action: "s3:GetObject",
                Resource: `arn:${partition}:s3:::${bucket}/${runnerSnapshots}` }] : []),
            {
                Effect: "Allow",
                Action: ["s3:GetBucketVersioning", "s3:GetLifecycleConfiguration"],
                Resource: `arn:${partition}:s3:::${bucket}`,
            },
        ],
    });
}

/**
 * One EC2 host with nested virtualization running the Open Workspaces
 * controller and runner. No inbound ports: reach it through SSM, or Tailscale
 * when an auth-key secret is given.
 */
export class Host extends pulumi.ComponentResource {
    public readonly instanceId: pulumi.Output<string>;
    public readonly privateIp: pulumi.Output<string>;
    public readonly availabilityZone: pulumi.Output<string>;
    public readonly dataVolumeId: pulumi.Output<string>;
    public readonly roleName: pulumi.Output<string>;
    public readonly securityGroupId: pulumi.Output<string>;
    /** SSM parameter holding the host's config; apply a change with `ow-aws apply`. */
    public readonly configParameter: pulumi.Output<string>;
    /** The component name: the tag Name `ow-aws --host` finds, and a runner's node ID. */
    public readonly hostName: string;

    constructor(name: string, args: HostArgs, opts?: pulumi.ComponentResourceOptions) {
        super("open-workspaces-aws:index:Host", name, {}, opts);
        this.hostName = name;
        const out = provision(this, name, args);
        this.instanceId = out.instanceId;
        this.privateIp = out.privateIp;
        this.availabilityZone = out.availabilityZone;
        this.dataVolumeId = out.dataVolumeId;
        this.roleName = out.roleName;
        this.securityGroupId = out.securityGroupId;
        this.configParameter = out.configParameter;
        this.registerOutputs(out);
    }
}

type HostOutputs = Pick<Host, "instanceId" | "privateIp" | "availabilityZone" | "dataVolumeId" | "roleName" | "securityGroupId" | "configParameter">;

/** The resources of a Host or a Runner, as children of `self`. */
function provision(self: pulumi.ComponentResource, name: string, args: HostArgs): HostOutputs {
        const parent = { parent: self };
        const controller = (args as Partial<RunnerArgs>).controller;
        // A runner keeps its snapshots under its own prefix of the shared store.
        const backupUrl = args.backupUrl && controller
            ? pulumi.output(args.backupUrl).apply((url) => `${url.replace(/\/+$/, "")}/runners/${nodeId(name)}`)
            : args.backupUrl;
        const hostRole = pulumi.output(args.role ?? "host").apply((role) => {
            if (!["host", "controller"].includes(role) || (controller && role !== "host")) throw new Error("invalid host role");
            return role;
        });
        const instanceType = args.instanceType ? pulumi.output(args.instanceType)
            : hostRole.apply((role) => role === "controller" ? "m7i.large" : "m8i-flex.xlarge");
        const tags = pulumi.output(args.tags ?? {});
        const named = tags.apply((t) => ({ ...t, Name: name }));

        const subnetId = args.subnetId
            ? pulumi.output(args.subnetId)
            : instanceType.apply(async (type) => {
                  const vpc = await aws.ec2.getVpc({ default: true }, parent);
                  const offerings = await aws.ec2.getInstanceTypeOfferings(
                      { locationType: "availability-zone", filters: [{ name: "instance-type", values: [type] }] },
                      parent,
                  );
                  const subnets = await aws.ec2.getSubnets(
                      {
                          filters: [
                              { name: "vpc-id", values: [vpc.id] },
                              { name: "default-for-az", values: ["true"] },
                              { name: "availability-zone", values: offerings.locations },
                          ],
                      },
                      parent,
                  );
                  if (subnets.ids.length === 0) throw new Error(`no default subnet offers ${type}; pass subnetId`);
                  return [...subnets.ids].sort()[0];
              });
        const subnet = aws.ec2.getSubnetOutput({ id: subnetId }, parent);
        const partition = aws.getPartitionOutput({}, parent).partition;

        const role = new aws.iam.Role(`${name}-host`, {
            assumeRolePolicy: JSON.stringify({
                Version: "2012-10-17",
                Statement: [{ Effect: "Allow", Principal: { Service: "ec2.amazonaws.com" }, Action: "sts:AssumeRole" }],
            }),
            tags,
        }, parent);
        new aws.iam.RolePolicyAttachment(`${name}-ssm`, {
            role: role.name,
            policyArn: pulumi.interpolate`arn:${partition}:iam::aws:policy/AmazonSSMManagedInstanceCore`,
        }, parent);
        if (args.tailscaleAuthKeySecretArn) {
            new aws.iam.RolePolicy(`${name}-tailscale-key`, {
                role: role.id,
                policy: pulumi.output(args.tailscaleAuthKeySecretArn).apply((arn) => JSON.stringify({
                    Version: "2012-10-17",
                    Statement: [{ Effect: "Allow", Action: "secretsmanager:GetSecretValue", Resource: arn }],
                })),
            }, parent);
        }
        if (args.storeGc) {
            if (controller || !args.storeUrl) throw new Error("storeGc needs a controller host and storeUrl");
            new aws.iam.RolePolicy(`${name}-store-gc`, { role: role.id,
                policy: pulumi.all([partition, args.storeUrl]).apply(([p, url]) => storeGcPolicy(p, url)) }, parent);
        }
        if (args.storeUrl) {
            new aws.iam.RolePolicy(`${name}-store`, { role: role.id,
                policy: pulumi.all([partition, args.storeUrl]).apply(([p, url]) => storeRunnerPolicy(p, url)) }, parent);
        }
        if (backupUrl) {
            new aws.iam.RolePolicy(`${name}-backup`, {
                role: role.id,
                policy: pulumi.all([partition, backupUrl]).apply(([p, url]) => backupPolicy(p, url, !controller)),
            }, parent);
        }
        // A runner's single-use join: the controller host writes it (`ow-aws runner-add`),
        // this host reads it once and overwrites it. No other host can read it.
        let runner = pulumi.output<RunnerConfig | undefined>(undefined);
        if (controller) {
            const join = new aws.secretsmanager.Secret(`${name}-join`, {
                name: `open-workspaces/${nodeId(name)}/join`,
                description: `Open Workspaces: single-use join of runner ${name} to ${controller.hostName}`,
                recoveryWindowInDays: 0,
                tags,
            }, parent);
            new aws.iam.RolePolicy(`${name}-join`, {
                role: role.id,
                policy: join.arn.apply((arn) => JSON.stringify({
                    Version: "2012-10-17",
                    Statement: [{ Effect: "Allow", Action: ["secretsmanager:GetSecretValue", "secretsmanager:PutSecretValue"], Resource: arn }],
                })),
            }, parent);
            new aws.iam.RolePolicy(`${name}-invite`, {
                role: controller.roleName,
                policy: join.arn.apply((arn) => JSON.stringify({
                    Version: "2012-10-17",
                    Statement: [{ Effect: "Allow", Action: "secretsmanager:PutSecretValue", Resource: arn }],
                })),
            }, parent);
            runner = join.arn.apply((arn): RunnerConfig | undefined => ({ node: name, controller: controller.hostName, joinSecretArn: arn }));
        }
        let directInterface: aws.ec2.NetworkInterface | undefined;
        if (args.tailscaleDirect) {
            if (!args.tailscaleAuthKeySecretArn) throw new Error("tailscaleDirect needs tailscaleAuthKeySecretArn");
            const directSubnet = aws.ec2.getSubnetOutput({ id: pulumi.output(args.tailscaleDirect).apply((direct) => direct.subnetId) }, parent);
            const checkedSubnet = pulumi.all([subnet, directSubnet]).apply(([host, direct]) => tailscaleDirectSubnet(host, direct));
            const directSg = new aws.ec2.SecurityGroup(`${name}-tailscale-direct`, {
                description: "Direct Tailscale WireGuard only",
                vpcId: subnet.vpcId,
                ingress: [{ protocol: "udp", fromPort: 41641, toPort: 41641, cidrBlocks: ["0.0.0.0/0"] }],
                // Peers can listen on any UDP port; STUN also uses this socket.
                egress: [{ protocol: "udp", fromPort: 0, toPort: 65535, cidrBlocks: ["0.0.0.0/0"] }],
                tags: named,
            }, parent);
            directInterface = new aws.ec2.NetworkInterface(`${name}-tailscale-direct`, {
                subnetId: checkedSubnet, securityGroups: [directSg.id], tags: named,
            }, parent);
            const address = new aws.ec2.Eip(`${name}-tailscale-direct`, { domain: "vpc", tags: named }, parent);
            new aws.ec2.EipAssociation(`${name}-tailscale-direct`, {
                allocationId: address.id, networkInterfaceId: directInterface.id,
            }, parent);
        }
        // Everything the host applies in place: user data only bootstraps (stage 0), so a
        // config or release change updates this parameter and never replaces the instance.
        const config = new aws.ssm.Parameter(`${name}-config`, {
            name: `/open-workspaces/${name}/config`,
            type: "String",
            tier: "Standard",
            insecureValue: pulumi
                .all([
                    pulumi.all([args.repository ?? DEFAULT_REPOSITORY, args.ref ?? `v${require("./package.json").version}`]),
                    pulumi.all([args.tailscaleAuthKeySecretArn ?? "", args.tailscaleHostname ?? name, args.tailscaleSsh ?? false]),
                    pulumi.all([args.dashboardLogins ?? [], args.developerImages ?? [], args.dashboardCapability ?? ""]),
                    pulumi.all([backupUrl ?? "", args.backupSchedule ?? "", args.idleMinutes ?? 30, args.vcpuOvercommit ?? 1, args.recoveryPoints ?? false]),
                    runner,
                    directInterface ? directInterface.macAddress : pulumi.output(""),
                    pulumi.all([args.storeUrl ?? "", args.imageArtifacts ?? {}]),
                    hostRole,
                ])
                .apply(([[repository, ref], [secretArn, hostname, tailscaleSsh], [logins, images, capability], [backupUrl, backupSchedule, idleMinutes, vcpuOvercommit, recoveryPoints], runner, directMac, [storeUrl, imageArtifacts], role]) =>
                    JSON.stringify(hostConfig({
                        role, repository, ref, tailscaleAuthKeySecretArn: secretArn, tailscaleHostname: hostname, tailscaleSsh,
                        dashboardLogins: logins, dashboardCapability: capability, developerImages: images, backupUrl, backupSchedule, idleMinutes, vcpuOvercommit, recoveryPoints, runner, directMac, storeUrl, imageArtifacts,
                    })),
                ),
            tags,
        }, parent);
        const configRead = new aws.iam.RolePolicy(`${name}-config`, {
            role: role.id,
            policy: config.arn.apply((arn) => JSON.stringify({
                Version: "2012-10-17",
                Statement: [{ Effect: "Allow", Action: "ssm:GetParameter", Resource: arn }],
            })),
        }, parent);
        const profile = new aws.iam.InstanceProfile(`${name}-host`, { role: role.name, tags }, parent);

        // No ingress rules: SSM and Tailscale both connect outbound.
        const securityGroup = new aws.ec2.SecurityGroup(`${name}-host`, {
            description: "Open Workspaces host: no inbound access",
            vpcId: subnet.vpcId,
            egress: [
                { protocol: "-1", fromPort: 0, toPort: 0, cidrBlocks: ["0.0.0.0/0"], ipv6CidrBlocks: ["::/0"] },
            ],
            tags: named,
        }, parent);

        // A separate volume survives host replacement (new AMI or ref).
        const dataVolume = new aws.ebs.Volume(`${name}-data`, {
            availabilityZone: subnet.availabilityZone,
            type: "gp3",
            size: args.dataVolumeSizeGib ?? 100,
            iops: args.dataVolumeIops,
            throughput: args.dataVolumeThroughput,
            encrypted: true,
            tags: named.apply((t) => ({ ...t, Name: `${name}-data` })),
        }, { ...parent, retainOnDelete: true });

        const amiId = args.amiId ?? aws.ssm.getParameterOutput({ name: UBUNTU_AMI_PARAMETER }, parent).value;
        // Stage 0 only: these values never change with the config or the release.
        const region = aws.getRegionOutput({}, parent).region;
        const userData = pulumi
            .all([dataVolume.id, config.name, region])
            .apply(([volumeId, parameter, regionName]) => renderUserData({
                OW_DATA_VOLUME_ID: volumeId,
                OW_CONFIG_PARAMETER: parameter,
                OW_REGION: regionName,
                OW_STAGE0: STAGE0,
            }));

        const instance = new aws.ec2.Instance(`${name}-host`, {
            ami: amiId,
            instanceType,
            subnetId,
            associatePublicIpAddress: args.associatePublicIpAddress ?? true,
            vpcSecurityGroupIds: [securityGroup.id],
            iamInstanceProfile: profile.name,
            cpuOptions: hostRole.apply((role) => role === "controller" ? undefined : { nestedVirtualization: "enabled" }),
            metadataOptions: { httpEndpoint: "enabled", httpTokens: "required", httpPutResponseHopLimit: 1 },
            rootBlockDevice: { volumeType: "gp3", volumeSize: 30, encrypted: true },
            userData,
            // Only a stage 0 change (bootstrap.sh) changes user data: then a replacement is honest.
            userDataReplaceOnChange: true,
            tags: named,
            volumeTags: named,
        }, { parent: self, deleteBeforeReplace: true, dependsOn: [configRead] });

        if (directInterface) {
            new aws.ec2.NetworkInterfaceAttachment(`${name}-tailscale-direct`, {
                instanceId: instance.id, networkInterfaceId: directInterface.id, deviceIndex: 1,
            }, parent);
        }

        new aws.ec2.VolumeAttachment(`${name}-data`, {
            instanceId: instance.id,
            volumeId: dataVolume.id,
            deviceName: "/dev/sdf",
            stopInstanceBeforeDetaching: true,
        }, { parent: self, deleteBeforeReplace: true });

        return {
            instanceId: instance.id,
            privateIp: instance.privateIp,
            availabilityZone: subnet.availabilityZone,
            dataVolumeId: dataVolume.id,
            roleName: role.name,
            securityGroupId: securityGroup.id,
            configParameter: config.name,
        };
}

/**
 * A runner host that joins another Host's controller: its own nested-virtualization
 * instance, Btrfs volume kept across replacement, role, backup prefix
 * (`backupUrl/runners/NAME`) and config parameter; a worker and a node agent, no
 * controller or dashboard. The component name is its node ID. Enrol it with
 * `ow-aws --host CONTROLLER runner-add NAME` (`ow-aws ci-post-up` on the runner does it).
 * Creating or changing it touches no other host: the controller's role only gains the
 * right to write this runner's join secret.
 */
// Extends ComponentResource itself, not Host: Pulumi's component analyzer finds only
// direct subclasses, so `pulumi package add` failed on 0.6.0 ("Failed to find ... Runner").
export class Runner extends pulumi.ComponentResource {
    public readonly instanceId: pulumi.Output<string>;
    public readonly privateIp: pulumi.Output<string>;
    public readonly availabilityZone: pulumi.Output<string>;
    public readonly dataVolumeId: pulumi.Output<string>;
    public readonly roleName: pulumi.Output<string>;
    public readonly securityGroupId: pulumi.Output<string>;
    /** SSM parameter holding the host's config; apply a change with `ow-aws apply`. */
    public readonly configParameter: pulumi.Output<string>;
    /** The component name: the tag Name `ow-aws --host` finds, and a runner's node ID. */
    public readonly hostName: string;

    constructor(name: string, args: RunnerArgs, opts?: pulumi.ComponentResourceOptions) {
        nodeId(name);
        super("open-workspaces-aws:index:Runner", name, {}, opts);
        this.hostName = name;
        const out = provision(this, name, args);
        this.instanceId = out.instanceId;
        this.privateIp = out.privateIp;
        this.availabilityZone = out.availabilityZone;
        this.dataVolumeId = out.dataVolumeId;
        this.roleName = out.roleName;
        this.securityGroupId = out.securityGroupId;
        this.configParameter = out.configParameter;
        this.registerOutputs(out);
    }
}
