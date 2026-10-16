import * as fs from "fs";
import * as path from "path";
import * as aws from "@pulumi/aws";
import * as pulumi from "@pulumi/pulumi";

export interface HostArgs {
    /** Nested-virtualization type: c8i, m8i, r8i or their flex variants. */
    instanceType?: pulumi.Input<string>;
    /** Subnet for the host. Default: the default VPC's default subnet in the first zone offering the type. */
    subnetId?: pulumi.Input<string>;
    /** A public IPv4 address gives egress without a NAT gateway. The security group still has no inbound rules. */
    associatePublicIpAddress?: pulumi.Input<boolean>;
    /** x86-64 Ubuntu 24.04 AMI. Default: Canonical's current image from SSM. A new image replaces the host, never the data volume. */
    amiId?: pulumi.Input<string>;
    /** Size of the gp3 Btrfs volume holding sources, assets and workspaces. */
    dataVolumeSizeGib?: pulumi.Input<number>;
    /** Git repository with the Open Workspaces sources. */
    repository?: pulumi.Input<string>;
    /** Git tag or commit to build on the host. Default: this package's release tag. */
    ref?: pulumi.Input<string>;
    /** Tags for every resource the component creates. */
    tags?: pulumi.Input<{ [key: string]: pulumi.Input<string> }>;
    /** Full ARN of a Secrets Manager secret holding a Tailscale auth key. Omit to skip Tailscale. */
    tailscaleAuthKeySecretArn?: pulumi.Input<string>;
    /** Tailnet hostname. Default: the component name. */
    tailscaleHostname?: pulumi.Input<string>;
    /** Enable Tailscale SSH, authorized by the tailnet policy. */
    tailscaleSsh?: pulumi.Input<boolean>;
    /**
     * Tailnet logins (for example `you@example.com`) allowed into the web dashboard.
     * The dashboard listens on loopback and is served to the tailnet only, through
     * `tailscale serve`. Needs `tailscaleAuthKeySecretArn`. Omit for no dashboard.
     */
    dashboardLogins?: pulumi.Input<pulumi.Input<string>[]>;
    /**
     * Developer images (`ubuntu` only for now) to build on the host after boot: the `dev`
     * user, SSH server and toolchains that SSH access and golden-image recipes need.
     * Built once per data volume, in the background. Default: none (Alpine only).
     */
    developerImages?: pulumi.Input<pulumi.Input<string>[]>;
}

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

    constructor(name: string, args: HostArgs, opts?: pulumi.ComponentResourceOptions) {
        super("open-workspaces-aws:index:Host", name, {}, opts);
        const parent = { parent: this };
        const instanceType = pulumi.output(args.instanceType ?? "m8i-flex.xlarge");
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
            encrypted: true,
            tags: named.apply((t) => ({ ...t, Name: `${name}-data` })),
        }, parent);

        const amiId = args.amiId ?? aws.ssm.getParameterOutput({ name: UBUNTU_AMI_PARAMETER }, parent).value;
        const userData = pulumi
            .all([
                dataVolume.id,
                args.repository ?? DEFAULT_REPOSITORY,
                args.ref ?? `v${require("./package.json").version}`,
                args.tailscaleAuthKeySecretArn ?? "",
                args.tailscaleHostname ?? name,
                args.tailscaleSsh ?? false,
                args.dashboardLogins ?? [],
                args.developerImages ?? [],
            ])
            .apply(([volumeId, repository, ref, secretArn, hostname, tailscaleSsh, logins, images]) => {
                if (logins.length > 0 && !secretArn) {
                    throw new Error("dashboardLogins needs tailscaleAuthKeySecretArn: the dashboard is served over the tailnet only");
                }
                return renderUserData({
                    OW_DATA_VOLUME_ID: volumeId,
                    OW_REPOSITORY: repository,
                    OW_REF: ref,
                    OW_TAILSCALE_SECRET_ARN: secretArn,
                    OW_TAILSCALE_HOSTNAME: hostname,
                    OW_TAILSCALE_SSH: tailscaleSsh ? "1" : "",
                    OW_DASHBOARD_LOGINS: dashboardLoginList(logins),
                    OW_DEVELOPER_IMAGES: developerImageList(images),
                });
            });

        const instance = new aws.ec2.Instance(`${name}-host`, {
            ami: amiId,
            instanceType,
            subnetId,
            associatePublicIpAddress: args.associatePublicIpAddress ?? true,
            vpcSecurityGroupIds: [securityGroup.id],
            iamInstanceProfile: profile.name,
            cpuOptions: { nestedVirtualization: "enabled" },
            metadataOptions: { httpEndpoint: "enabled", httpTokens: "required", httpPutResponseHopLimit: 1 },
            rootBlockDevice: { volumeType: "gp3", volumeSize: 30, encrypted: true },
            userData,
            userDataReplaceOnChange: true,
            tags: named,
            volumeTags: named,
        }, { parent: this, deleteBeforeReplace: true });

        new aws.ec2.VolumeAttachment(`${name}-data`, {
            instanceId: instance.id,
            volumeId: dataVolume.id,
            deviceName: "/dev/sdf",
            stopInstanceBeforeDetaching: true,
        }, { parent: this, deleteBeforeReplace: true });

        this.instanceId = instance.id;
        this.privateIp = instance.privateIp;
        this.availabilityZone = subnet.availabilityZone;
        this.dataVolumeId = dataVolume.id;
        this.roleName = role.name;
        this.securityGroupId = securityGroup.id;
        this.registerOutputs({
            instanceId: this.instanceId,
            privateIp: this.privateIp,
            availabilityZone: this.availabilityZone,
            dataVolumeId: this.dataVolumeId,
            roleName: this.roleName,
            securityGroupId: this.securityGroupId,
        });
    }
}
