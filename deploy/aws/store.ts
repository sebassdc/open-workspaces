import * as aws from "@pulumi/aws";
import * as pulumi from "@pulumi/pulumi";

export interface ArtifactStoreArgs {
    /** Bucket name. Default: a name Pulumi derives from the component name. */
    bucketName?: pulumi.Input<string>;
    /** Prefix inside the bucket, for example the host name. `url` is `s3://BUCKET/PREFIX`. */
    prefix: pulumi.Input<string>;
    /** Days before a catalog backup expires. Live artifacts never age-expire. Default: 30. */
    expireDays?: pulumi.Input<number>;
    /** Days a noncurrent object is kept for undo. Minimum/default: 7. */
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
 * Lifecycle rules. Age expiry covers only catalog backups. Snapshots, runners and the store retain
 * current objects until controller GC removes unreferenced data:
 * a content store under `PREFIX/store/` keeps objects as long as a catalog references
 * them, so an age rule there would delete live artifacts.
 */
export function storeLifecycleRules(prefix: string, expireDays = 30, noncurrentDays = 7) {
    for (const [name, days] of [["expireDays", expireDays], ["noncurrentDays", noncurrentDays]] as const) {
        if (!Number.isInteger(days) || days < (name === "noncurrentDays" ? 7 : 1)) throw new Error(`${name} must be a whole number of days, at least ${name === "noncurrentDays" ? 7 : 1}`);
    }
    const root = storePrefix(prefix);
    return [
        { id: "expire-catalog", status: "Enabled", filter: { prefix: `${root}/catalog/` },
            expiration: { days: expireDays }, noncurrentVersionExpiration: { noncurrentDays } },
        ...["snapshots", "runners", "store"].map((part) => ({
            id: `undo-${part}`, status: "Enabled", filter: { prefix: `${root}/${part}/` },
            noncurrentVersionExpiration: { noncurrentDays },
        })),
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

/** Conditional creation on the content namespace; multipart setup/parts remain allowed. */
export function storeBucketPolicy(bucketArn: string, prefix: string): string {
    const policy = JSON.parse(tlsOnlyPolicy(bucketArn));
    policy.Statement.push({
        Sid: "StoreIsWriteOnce", Effect: "Deny", Principal: "*", Action: "s3:PutObject",
        Resource: `${bucketArn}/${storePrefix(prefix)}/store/v1/*`,
        Condition: { Null: { "s3:if-none-match": "true" }, Bool: { "s3:ObjectCreationOperation": "true" } },
    });
    return JSON.stringify(policy);
}

/** Image agents can read/write exact objects, never list/delete. */
export function storeRunnerPolicy(partition: string, url: string): string {
    const match = /^s3:\/\/([a-z0-9][a-z0-9.-]{1,61}[a-z0-9])\/(.+)$/.exec(url);
    if (!match) throw new Error("storeUrl must be s3://bucket/prefix");
    const bucket = `arn:${partition}:s3:::${match[1]}`;
    const prefix = storePrefix(match[2]);
    return JSON.stringify({ Version: "2012-10-17", Statement: [
        { Effect: "Allow", Action: ["s3:GetObject", "s3:PutObject", "s3:AbortMultipartUpload"], Resource: `${bucket}/${prefix}/store/v1/u/*` },
        { Effect: "Allow", Action: ["s3:GetBucketVersioning", "s3:GetLifecycleConfiguration", "s3:GetBucketPolicy"], Resource: bucket },
    ] });
}

/** Opt-in controller collector: candidate listing and current-version deletes only. */
export function storeGcPolicy(partition: string, url: string): string {
    storeRunnerPolicy(partition, url);
    const match = /^s3:\/\/([^/]+)\/(.+)$/.exec(url)!;
    const bucket = `arn:${partition}:s3:::${match[1]}`;
    const root = `${storePrefix(match[2])}/store/v1`;
    return JSON.stringify({ Version: "2012-10-17", Statement: [
        { Effect: "Allow", Action: ["s3:GetBucketVersioning", "s3:GetLifecycleConfiguration", "s3:GetBucketPolicy"], Resource: bucket },
        { Effect: "Allow", Action: "s3:ListBucket", Resource: bucket,
            Condition: { StringLike: { "s3:prefix": `${root}/u/*` } } },
        { Effect: "Allow", Action: ["s3:GetObject", "s3:DeleteObject"], Resource: `${bucket}/${root}/u/*` },
        { Effect: "Allow", Action: ["s3:PutObject", "s3:GetObject"], Resource: `${bucket}/${root}/gc/runs/*` },
        { Effect: "Deny", Action: "s3:DeleteObjectVersion", Resource: `${bucket}/*` },
    ] });
}

/**
 * A private S3 bucket for backups (and later a content store): public access blocked,
 * versioning on, TLS only, write-once store objects, and age expiry on `PREFIX/catalog/`
 * only. Pass `url` to a Host's `backupUrl` and opt-in `storeUrl`.
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
        new aws.s3.BucketPolicy(name, { bucket: bucket.id, policy: pulumi.all([bucket.arn, prefix]).apply(([arn, p]) => storeBucketPolicy(arn, p)) }, parent);

        this.bucketName = bucket.bucket;
        this.bucketArn = bucket.arn;
        this.url = pulumi.interpolate`s3://${bucket.bucket}/${prefix}`;
        this.registerOutputs({ bucketName: this.bucketName, bucketArn: this.bucketArn, url: this.url });
    }
}
