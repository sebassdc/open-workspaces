import * as pulumi from "@pulumi/pulumi";
import { Host } from "../../index";

// Inputs come from the environment only; run.sh documents each variable.
const env = (key: string) => process.env[key] || undefined;

const host = new Host("ow-smoke", {
    instanceType: env("OW_INSTANCE_TYPE"),
    subnetId: env("OW_SUBNET_ID"),
    repository: env("OW_REPOSITORY"),
    ref: env("OW_REF") ?? "main",
    dataVolumeSizeGib: env("OW_DATA_GIB") ? Number(env("OW_DATA_GIB")) : undefined,
    tailscaleAuthKeySecretArn: env("OW_TAILSCALE_SECRET_ARN"),
    tailscaleSsh: env("OW_TAILSCALE_SSH") === "1",
    tags: { project: "open-workspaces", "open-workspaces:stack": pulumi.getStack() },
});

export const instanceId = host.instanceId;
export const availabilityZone = host.availabilityZone;
