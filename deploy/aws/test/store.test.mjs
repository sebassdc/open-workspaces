import assert from "node:assert/strict";
import { test } from "node:test";
import { storeLifecycleRules, tlsOnlyPolicy } from "../store.ts";

test("age expiry covers only the backup prefixes, never store/", () => {
    const rules = storeLifecycleRules("ow-host/", 30, 7);
    const expiring = rules.filter((rule) => rule.expiration || rule.noncurrentVersionExpiration);
    assert.deepEqual(expiring.map((rule) => rule.filter.prefix), ["ow-host/catalog/", "ow-host/snapshots/", "ow-host/runners/"]);
    assert.ok(expiring.every((rule) => rule.expiration.days === 30 && rule.noncurrentVersionExpiration.noncurrentDays === 7));
    // The one bucket-wide rule only drops unfinished multipart uploads.
    const wide = rules.filter((rule) => !rule.filter.prefix);
    assert.deepEqual(wide.map((rule) => Object.keys(rule).sort()), [["abortIncompleteMultipartUpload", "filter", "id", "status"]]);
    assert.ok(!JSON.stringify(rules).includes("store/"));
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
    assert.deepEqual(JSON.parse(byType["aws:s3/bucketPolicy:BucketPolicy"].policy), JSON.parse(tlsOnlyPolicy("arn:aws:s3:::example-backups")));
    const rules = byType["aws:s3/bucketLifecycleConfiguration:BucketLifecycleConfiguration"].rules;
    assert.deepEqual(rules.map((rule) => rule.filter.prefix ?? null), ["ow-host/catalog/", "ow-host/snapshots/", "ow-host/runners/", null]);
});
