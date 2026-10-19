import assert from "node:assert/strict";
import { test } from "node:test";
import { storeLifecycleRules, tlsOnlyPolicy, storeBucketPolicy, storeRunnerPolicy, storeGcPolicy } from "../store.ts";

test("only catalog backups expire by age; data keeps seven-day noncurrent undo", () => {
    const rules = storeLifecycleRules("ow-host/", 30, 7);
    assert.deepEqual(rules.filter((r) => r.expiration).map((r) => r.filter.prefix), ["ow-host/catalog/"]);
    assert.deepEqual(rules.filter((r) => r.noncurrentVersionExpiration).map((r) => r.filter.prefix), ["ow-host/catalog/", "ow-host/snapshots/", "ow-host/runners/", "ow-host/store/"]);
    assert.ok(rules.filter((r) => r.noncurrentVersionExpiration).every((r) => r.noncurrentVersionExpiration.noncurrentDays === 7));
    assert.deepEqual(rules.filter((r) => !r.filter.prefix).map((r) => Object.keys(r).sort()), [["abortIncompleteMultipartUpload", "filter", "id", "status"]]);
    assert.throws(() => storeLifecycleRules("p", 30, 6), /whole number/);
});

test("store prefixes and retention days are checked", () => {
    for (const prefix of ["", "/x", "a/../b", "a//b", "a b", "./a"]) {
        assert.throws(() => storeLifecycleRules(prefix), /invalid store prefix/, prefix);
    }
    for (const days of [0, 1.5, -1]) {
        assert.throws(() => storeLifecycleRules("p", days), /whole number/);
        assert.throws(() => storeLifecycleRules("p", 30, days), /whole number/);
    }
});

test("the bucket policy denies every request without TLS", () => {
    const policy = JSON.parse(tlsOnlyPolicy("arn:aws:s3:::b"));
    assert.deepEqual(policy.Statement, [{
        Sid: "TlsOnly",
        Effect: "Deny",
        Principal: "*",
        Action: "s3:*",
        Resource: ["arn:aws:s3:::b", "arn:aws:s3:::b/*"],
        Condition: { Bool: { "aws:SecureTransport": "false" } },
    }]);
});

test("the component creates a private, versioned, TLS-only bucket with prefix-scoped expiry", async () => {
    const pulumi = await import("@pulumi/pulumi");
    const created = [];
    await pulumi.runtime.setMocks({
        newResource: ({ type, name, inputs }) => {
            created.push({ type, name, inputs });
            const state = type === "aws:s3/bucket:Bucket" ? { bucket: inputs.bucket, arn: `arn:aws:s3:::${inputs.bucket}` } : {};
            return { id: `${name}-id`, state: { ...inputs, ...state } };
        },
        call: ({ inputs }) => inputs,
    }, "project", "stack", false);
    const { ArtifactStore } = await import("../store.ts");
    const store = new ArtifactStore("backups", { bucketName: "example-backups", prefix: "ow-host", tags: { project: "ow" } });
    const url = await new Promise((resolve) => store.url.apply(resolve));
    assert.equal(url, "s3://example-backups/ow-host");
    // Children register asynchronously, once their inputs resolve.
    for (let i = 0; created.length < 5 && i < 100; i++) await new Promise((resolve) => setTimeout(resolve, 10));
    const byType = Object.fromEntries(created.map((r) => [r.type, r.inputs]));
    assert.deepEqual(byType["aws:s3/bucket:Bucket"].tags, { project: "ow" });
    assert.deepEqual(
        { ...byType["aws:s3/bucketPublicAccessBlock:BucketPublicAccessBlock"], bucket: undefined },
        { bucket: undefined, blockPublicAcls: true, blockPublicPolicy: true, ignorePublicAcls: true, restrictPublicBuckets: true },
    );
    assert.equal(byType["aws:s3/bucketVersioning:BucketVersioning"].versioningConfiguration.status, "Enabled");
    assert.deepEqual(JSON.parse(byType["aws:s3/bucketPolicy:BucketPolicy"].policy), JSON.parse(storeBucketPolicy("arn:aws:s3:::example-backups", "ow-host")));
    const rules = byType["aws:s3/bucketLifecycleConfiguration:BucketLifecycleConfiguration"].rules;
    assert.deepEqual(rules.map((rule) => rule.filter.prefix ?? null), ["ow-host/catalog/", "ow-host/snapshots/", "ow-host/runners/", "ow-host/store/", null]);
});


test("store creation is conditional and image roles cannot list or delete", () => {
    const policy = JSON.parse(storeBucketPolicy("arn:aws:s3:::bucket", "prefix"));
    const once = policy.Statement.find((s) => s.Sid === "StoreIsWriteOnce");
    assert.equal(once.Resource, "arn:aws:s3:::bucket/prefix/store/v1/*");
    assert.deepEqual(once.Condition, { Null: { "s3:if-none-match": "true" }, Bool: { "s3:ObjectCreationOperation": "true" } });
    const runner = JSON.parse(storeRunnerPolicy("aws", "s3://bucket/prefix"));
    assert.equal(runner.Statement[0].Resource, "arn:aws:s3:::bucket/prefix/store/v1/u/*");
    const actions = runner.Statement.flatMap((s) => s.Action);
    assert.ok(!actions.some((a) => /Delete|List/.test(a)));
    assert.ok(actions.includes("s3:AbortMultipartUpload") && actions.includes("s3:GetBucketVersioning"));
    for (const url of ["s3://bucket", "s3://bucket/a/../b", "s3://bucket/a//b"]) assert.throws(() => storeRunnerPolicy("aws", url));
});

test("GC role can only enumerate upload keys and create delete markers in that prefix", () => {
    const policy = JSON.parse(storeGcPolicy("aws", "s3://bucket/root"));
    const list = policy.Statement.find(s => s.Action === "s3:ListBucket");
    assert.deepEqual(list.Condition, { StringLike: { "s3:prefix": "root/store/v1/u/*" } });
    const deletion = policy.Statement.find(s => s.Effect === "Allow" && s.Action.includes("s3:DeleteObject"));
    assert.equal(deletion.Resource, "arn:aws:s3:::bucket/root/store/v1/u/*");
    assert.deepEqual(policy.Statement.find(s => s.Effect === "Deny"), {
        Effect: "Deny", Action: "s3:DeleteObjectVersion", Resource: "arn:aws:s3:::bucket/*",
    });
    assert.ok(!policy.Statement.filter(s => s.Effect === "Allow").some(s => s.Action.includes("DeleteObjectVersion")));
    assert.throws(() => storeGcPolicy("aws", "s3://bucket/root/../other"));
});
