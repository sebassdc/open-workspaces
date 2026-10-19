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
        assert.equal(elements.get("metric-limit").textContent, `of ${label} capacity`);
        assert.equal(elements.get("metric-reserved").textContent, allocated);
        assert.equal(elements.get("capacity-text").textContent, `${allocated} / ${label}`);
        assert.equal(elements.get("capacity").hidden, !(capacity > 0));
        if (capacity > 0) {
            assert.equal(elements.get("capacity").max, capacity);
            assert.equal(elements.get("capacity").value, reserved);
        }
    }
});
