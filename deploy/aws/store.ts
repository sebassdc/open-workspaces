import * as aws from "@pulumi/aws";
import * as pulumi from "@pulumi/pulumi";

export interface ArtifactStoreArgs {
    /** Bucket name. Default: a name Pulumi derives from the component name. */
    bucketName?: pulumi.Input<string>;
    /** Prefix inside the bucket, for example the host name. `url` is `s3://BUCKET/PREFIX`. */
    prefix: pulumi.Input<string>;
    /** Days before an object under `PREFIX/catalog/` or `PREFIX/snapshots/` expires. Default: 30. */
    expireDays?: pulumi.Input<number>;
    /** Days an overwritten copy under those prefixes is kept. Default: 7. */
    noncurrentDays?: pulumi.Input<number>;
    /** Tags for every resource the component creates. */
    tags?: pulumi.Input<{ [key: string]: pulumi.Input<string> }>;
}

/** Checks a store prefix: path segments of letters, digits, `.`, `_` and `-`, no `.` or `..`. */
export function storePrefix(prefix: string): string {
    const trimmed = prefix.replace(/\/+$/, "");
    if (!/^[A-Za-z0-9_-][A-Za-z0-9._-]*(?:\/[A-Za-z0-9_-][A-Za-z0-9._-]*)*$/.test(trimmed)
        || trimmed.split("/").some((part) => part === "." || part === "..")) {
        throw new Error(`invalid store prefix: ${JSON.stringify(prefix)}`);
    }
    return trimmed;
}

/**
 * Lifecycle rules. Age expiry covers only the backup prefixes (`catalog/`, `snapshots/`):
 * a content store under `PREFIX/store/` keeps objects as long as a catalog references
 * them, so an age rule there would delete live artifacts.
 */
export function storeLifecycleRules(prefix: string, expireDays = 30, noncurrentDays = 7) {
    for (const [name, days] of [["expireDays", expireDays], ["noncurrentDays", noncurrentDays]] as const) {
        if (!Number.isInteger(days) || days < 1) throw new Error(`${name} must be a whole number of days, at least 1`);
    }
    const root = storePrefix(prefix);
    return [
        ...["catalog", "snapshots"].map((part) => ({
            id: `expire-${part}`,
            status: "Enabled",
            filter: { prefix: `${root}/${part}/` },
            expiration: { days: expireDays },
            noncurrentVersionExpiration: { noncurrentDays },
        })),
        // Parts of an upload that never finished are never live data.
        { id: "abort-incomplete-uploads", status: "Enabled", filter: {}, abortIncompleteMultipartUpload: { daysAfterInitiation: 1 } },
    ];
}

/** Bucket policy that refuses every request not made over TLS. */
export function tlsOnlyPolicy(bucketArn: string): string {
    return JSON.stringify({
        Version: "2012-10-17",
        Statement: [{
            Sid: "TlsOnly",
            Effect: "Deny",
            Principal: "*",
            Action: "s3:*",
            Resource: [bucketArn, `${bucketArn}/*`],
            Condition: { Bool: { "aws:SecureTransport": "false" } },
        }],
    });
}

/**
 * A private S3 bucket for backups (and later a content store): public access blocked,
 * versioning on, TLS only, and age expiry on `PREFIX/catalog/` and `PREFIX/snapshots/`
 * only. Pass `url` to a Host's `backupUrl`.
 */
export class ArtifactStore extends pulumi.ComponentResource {
    public readonly bucketName: pulumi.Output<string>;
    public readonly bucketArn: pulumi.Output<string>;
    public readonly url: pulumi.Output<string>;

    constructor(name: string, args: ArtifactStoreArgs, opts?: pulumi.ComponentResourceOptions) {
        super("open-workspaces-aws:index:ArtifactStore", name, {}, opts);
        const parent = { parent: this };
        const prefix = pulumi.output(args.prefix).apply(storePrefix);
        const tags = args.tags ?? {};

        const bucket = new aws.s3.Bucket(name, { bucket: args.bucketName, tags }, parent);
        new aws.s3.BucketPublicAccessBlock(name, {
            bucket: bucket.id,
            blockPublicAcls: true,
            blockPublicPolicy: true,
            ignorePublicAcls: true,
            restrictPublicBuckets: true,
        }, parent);
        new aws.s3.BucketVersioning(name, {
            bucket: bucket.id,
            versioningConfiguration: { status: "Enabled" },
        }, parent);
        new aws.s3.BucketLifecycleConfiguration(name, {
            bucket: bucket.id,
            rules: pulumi.all([prefix, args.expireDays ?? 30, args.noncurrentDays ?? 7])
                .apply(([p, expire, noncurrent]) => storeLifecycleRules(p, expire, noncurrent)),
        }, parent);
        new aws.s3.BucketPolicy(name, { bucket: bucket.id, policy: bucket.arn.apply(tlsOnlyPolicy) }, parent);

        this.bucketName = bucket.bucket;
        this.bucketArn = bucket.arn;
        this.url = pulumi.interpolate`s3://${bucket.bucket}/${prefix}`;
        this.registerOutputs({ bucketName: this.bucketName, bucketArn: this.bucketArn, url: this.url });
    }
}
