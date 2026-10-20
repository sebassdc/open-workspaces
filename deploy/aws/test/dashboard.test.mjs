import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { test } from "node:test";

// Run the shipped renderer; network refresh and event handlers stay idle.
test("dashboard preserves zero capacity and follows enrolled capacity on refresh", () => {
    const node = () => ({ textContent: "", addEventListener() {}, append() {}, replaceChildren() {} });
    const elements = new Map();
    const document = {
        getElementById(id) {
            if (!elements.has(id)) elements.set(id, node());
            return elements.get(id);
        },
        createElement: node,
        querySelectorAll: () => [],
    };
    const context = vm.createContext({
        document,
        window: { addEventListener() {} },
        location: { origin: "http://127.0.0.1" },
        fetch: () => new Promise(() => {}),
        setInterval() {},
    });
    vm.runInContext(readFileSync(new URL("../../../crates/ow/ui/app.js", import.meta.url), "utf8"), context);
    for (const [capacity, reserved, label, allocated] of [
        [0, 0, "0 MiB", "0 MiB"],
        [4096, 1024, "4.0 GiB", "1.0 GiB"],
        [12288, 2048, "12.0 GiB", "2.0 GiB"],
        [0, 0, "0 MiB", "0 MiB"],
        [undefined, 0, "0 MiB", "0 MiB"],
    ]) {
        context.fixture = { workspaces: [], snapshots: [], nodes: [], stats: {
            limit_memory_mib: capacity, reserved_memory_mib: reserved,
        } };
        vm.runInContext("data = fixture; render()", context);
        // Without owner_limits the counter shows zeros, never the pool total.
        assert.equal(elements.get("metric-reserved").textContent, "0 MiB of 0 MiB");
        assert.equal(elements.get("capacity-text").textContent, `${allocated} / ${label}`);
        assert.equal(elements.get("capacity").hidden, !(capacity > 0));
        if (capacity > 0) {
            assert.equal(elements.get("capacity").max, capacity);
            assert.equal(elements.get("capacity").value, reserved);
        }
    }
});

// The counter follows the owner's caps, not pool-wide capacity.
test("dashboard counter shows owner usage against owner limits", () => {
    const node = () => ({ textContent: "", addEventListener() {}, append() {}, replaceChildren() {} });
    const elements = new Map();
    const document = {
        getElementById(id) {
            if (!elements.has(id)) elements.set(id, node());
            return elements.get(id);
        },
        createElement: node,
        querySelectorAll: () => [],
    };
    const context = vm.createContext({ document, window: { addEventListener() {} }, location: { origin: "http://127.0.0.1" }, fetch: () => new Promise(() => {}), setInterval() {} });
    vm.runInContext(readFileSync(new URL("../../../crates/ow/ui/app.js", import.meta.url), "utf8"), context);
    context.fixture = { workspaces: [], snapshots: [], nodes: [], stats: { limit_memory_mib: 65536, reserved_memory_mib: 512 },
        owner_limits: { caps: { memory_mib: 2048, cpus: 2, disk_gib: 64, boxes: 2 }, usage: { memory_mib: 1024, cpus: 1, disk_gib: 40, boxes: 1 } } };
    vm.runInContext("data = fixture; render()", context);
    assert.equal(elements.get("metric-reserved").textContent, "1.0 GiB of 2.0 GiB");
    assert.equal(elements.get("metric-limit").textContent, "1 of 2 machines · 1 of 2 vCPU · 40 of 64 GiB disk");
    assert.equal(elements.get("capacity-text").textContent, "512 MiB / 64.0 GiB");
});
